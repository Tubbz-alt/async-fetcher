#[macro_use]
extern crate derive_new;
#[macro_use]
extern crate log;
#[macro_use]
extern crate thiserror;

mod range;

use std::{
    fmt::Debug,
    io,
    path::Path,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use async_std::{
    fs::{self, File},
    io::copy,
    prelude::*,
};

use chrono::{DateTime, Utc};
use filetime::FileTime;
use futures::{
    channel::mpsc,
    stream::{FuturesOrdered, StreamExt},
};
use http::StatusCode;
use http_client::native::NativeClient;
use numtoa::NumToA;
use surf::{
    headers::Headers, middleware::HttpClient, Client, Exception, Request, Response,
};

/// An error from the asynchronous file fetcher.
#[derive(Debug, Error)]
pub enum Error {
    #[error("http client error")]
    Client(#[from] Exception),
    #[error("unable to concatenate fetched parts")]
    Concatenate(#[source] io::Error),
    #[error("unable to create file")]
    FileCreate(#[source] io::Error),
    #[error("unable to set timestamp on {:?}", _0)]
    FileTime(Arc<Path>, #[source] io::Error),
    #[error("content length is an invalid range")]
    InvalidRange(#[source] io::Error),
    #[error("unable to remove file with bad metadata")]
    MetadataRemove(#[source] io::Error),
    #[error("destination has no file name")]
    Nameless,
    #[error("unable to open fetched part")]
    OpenPart(Arc<Path>, #[source] io::Error),
    #[error("destination lacks parent")]
    Parentless,
    #[error("connection timed out")]
    TimedOut,
    #[error("error writing to file")]
    Write(#[source] io::Error),
    #[error("failed to rename partial to destination")]
    Rename(#[source] io::Error),
    #[error("server responded with an error: {}", _0)]
    Status(StatusCode),
}

/// Information about a source being fetched.
pub struct Source {
    /// URLs whereby the file can be found.
    pub urls: Arc<[Box<str>]>,
    /// Where the file shall ultimately be fetched to.
    pub dest: Arc<Path>,
    /// Optional location to store the partial file
    pub part: Option<Arc<Path>>,
}

/// Events which are submitted by the fetcher.
pub enum FetchEvent {
    /// States that we know the length of the file being fetched.
    ContentLength(Arc<Path>, u64),
    /// Contains the result of a fetched file.
    Fetched(Arc<Path>, Result<(), Error>),
    /// Notifies that a file is being fetched.
    Fetching(Arc<Path>),
    /// Reports the amount of bytes that have been read for a file.
    Progress(Arc<Path>, usize),
    /// Reports that a part of a file is being fetched.
    PartFetching(Arc<Path>, u16),
    /// Reports that a part has been fetched.
    PartFetched(Arc<Path>, u16),
}

/// An asynchronous file fetcher for clients fetching files.
///
/// The futures generated by the fetcher are compatible with single and multi-threaded
/// runtimes, allowing you to choose between the runtime that works best for your
/// application. A single-threaded runtime is generally recommended for fetching files,
/// as your network connection is unlikely to be faster than a single CPU core.
#[derive(new)]
pub struct Fetcher<C: HttpClient> {
    client: Client<C>,

    /// The number of files to fetch simultaneously.
    #[new(default)]
    concurrent_files: Option<usize>,

    /// The number of concurrent connections to sustain per file being fetched.
    #[new(default)]
    connections_per_file: Option<u16>,

    /// The maximum size of a part file when downloading in parts.
    #[new(default)]
    part_size: Option<u64>,

    /// The time to wait between chunks before giving up.
    #[new(default)]
    timeout: Option<Duration>,

    /// Holds a sender for submitting events to.
    #[new(default)]
    events: Option<Arc<mpsc::UnboundedSender<FetchEvent>>>,
}

impl Default for Fetcher<NativeClient> {
    fn default() -> Self { Self::new(Client::new()) }
}

impl<C: HttpClient> Fetcher<C> {
    /// The number of files to fetch simultaneously.
    pub fn concurrent_files(mut self, concurrent: usize) -> Self {
        self.concurrent_files = if concurrent > 1 { Some(concurrent) } else { None };

        self
    }

    /// The maximum number of connections to sustain concurrently per file.
    pub fn connections_per_file(mut self, connections: u16) -> Self {
        self.connections_per_file =
            if connections > 1 { Some(connections) } else { None };

        self
    }

    /// Attaches a sender, so the caller may receive events from the fetcher.
    pub fn events(mut self, sender: mpsc::UnboundedSender<FetchEvent>) -> Self {
        self.events = Some(Arc::new(sender));
        self
    }

    /// The maximum size of a part file when downloading in parts.
    pub fn part_size(mut self, bytes: u64) -> Self {
        self.part_size = if bytes == 0 { None } else { Some(bytes) };

        self
    }

    /// Amount of time to wait between a read before giving up.
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout = Some(duration);
        self
    }

    /// Request a file from one or more URIs.
    ///
    /// At least one URI must be provided as a source for the file. Each additional URI
    /// serves as a mirror for failover and load-balancing purposes.
    pub async fn request(
        self: Arc<Self>,
        uris: Arc<[Box<str>]>,
        to: Arc<Path>,
    ) -> Result<(), Error> {
        let mut modified = None;
        let mut length = None;
        let mut if_modified_since = None;

        // If the file already exists, validate that it is the same.
        if to.exists() {
            if let Some(mut response) = head(&self.client, &*uris[0]).await? {
                let headers = &(response.headers());
                let content_length = content_length(headers);
                modified = last_modified(headers);

                if let (Some(content_length), Some(last_modified)) =
                    (content_length, modified)
                {
                    match fs::metadata(to.as_ref()).await {
                        Ok(metadata) => {
                            let modified = metadata.modified().map_err(Error::Write)?;
                            let ts = modified
                                .duration_since(UNIX_EPOCH)
                                .expect("time went backwards");

                            if metadata.len() == content_length
                                && ts.as_secs() == last_modified.timestamp() as u64
                            {
                                return Ok(());
                            }

                            if_modified_since =
                                Some(DateTime::<Utc>::from(modified).to_rfc2822());
                            length = Some(content_length);
                        }
                        Err(why) => {
                            error!("failed to fetch metadata of {:?}: {}", to, why);
                            fs::remove_file(to.as_ref())
                                .await
                                .map_err(Error::MetadataRemove)?;
                        }
                    }
                }
            }
        }

        // If set, this will use multiple connections to download a file in parts.
        if let Some(tasks) = self.connections_per_file {
            if let Some(mut response) = head(&self.client, &*uris[0]).await? {
                let headers = &(response.headers());
                modified = last_modified(headers);
                let length = match length {
                    Some(length) => Some(length),
                    None => content_length(headers),
                };

                if let Some(length) = length {
                    if supports_range(&self.client, &*uris[0], length).await? {
                        if let Some(sender) = self.events.as_ref() {
                            let _ = sender.unbounded_send(FetchEvent::ContentLength(
                                to.clone(),
                                length,
                            ));
                        }

                        return self.get_many(length, tasks, uris, to, modified).await;
                    }
                }
            }
        }

        let mut request = self.client.get(&*uris[0]);
        if let Some(modified_since) = if_modified_since {
            request = request.set_header("if-modified-since", modified_since.as_str());
        }

        let path =
            match self.get(&mut modified, request, to.clone(), to.clone(), None).await {
                Ok(path) => path,
                // Server does not support if-modified-since
                Err(Error::Status(StatusCode::NOT_IMPLEMENTED)) => {
                    let request = self.client.get(&*uris[0]);
                    self.get(&mut modified, request, to.clone(), to, None).await?
                }
                Err(why) => return Err(why),
            };

        if let Some(modified) = modified {
            let filetime = FileTime::from_unix_time(modified.timestamp(), 0);
            filetime::set_file_times(&path, filetime, filetime)
                .map_err(move |why| Error::FileTime(path, why))?;
        }

        Ok(())
    }

    /// Requests many files from many URIs.
    pub async fn from_stream<S>(self: Arc<Self>, sources: S) -> Result<(), Error>
    where
        S: Stream<Item = Source> + Unpin + Send + 'static,
    {
        let _ = sources
            .for_each_concurrent(self.concurrent_files.unwrap_or(4), |source| {
                let fetcher = self.clone();

                async move {
                    if let Some(sender) = fetcher.events.as_ref() {
                        let _ = sender
                            .unbounded_send(FetchEvent::Fetching(source.dest.clone()));
                    }

                    let result = match source.part {
                        Some(part) => {
                            match fetcher.clone().request(source.urls, part.clone()).await
                            {
                                Ok(()) => fs::rename(&*part, &*source.dest)
                                    .await
                                    .map_err(Error::Rename),
                                Err(why) => Err(why),
                            }
                        }
                        None => {
                            fetcher
                                .clone()
                                .request(source.urls, source.dest.clone())
                                .await
                        }
                    };

                    if let Some(sender) = fetcher.events.as_ref() {
                        let _ = sender.unbounded_send(FetchEvent::Fetched(
                            source.dest.clone(),
                            result,
                        ));
                    }
                }
            })
            .await;

        Ok(())
    }

    async fn get(
        &self,
        modified: &mut Option<DateTime<Utc>>,
        request: Request<C>,
        to: Arc<Path>,
        dest: Arc<Path>,
        length: Option<u64>,
    ) -> Result<Arc<Path>, Error> {
        let mut file = File::create(to.as_ref()).await.map_err(Error::FileCreate)?;

        if let Some(length) = length {
            file.set_len(length).await.map_err(Error::Write)?;
        }

        let response = &mut validate(if let Some(duration) = self.timeout {
            timed(duration, async { request.await.map_err(Error::from) }).await??
        } else {
            request.await?
        })?;

        if modified.is_none() {
            *modified = last_modified(&(response.headers()));
        }

        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(to);
        }

        let buffer = &mut [0u8; 8 * 1024];
        let mut read;

        loop {
            let reader = async { response.read(buffer).await.map_err(Error::Write) };

            read = match self.timeout {
                Some(duration) => timed(duration, reader).await??,
                None => reader.await?,
            };

            if read != 0 {
                if let Some(sender) = self.events.as_ref() {
                    let _ =
                        sender.unbounded_send(FetchEvent::Progress(dest.clone(), read));
                }

                file.write_all(&buffer[..read]).await.map_err(Error::Write)?;
            } else {
                break;
            }
        }

        Ok(to)
    }

    async fn get_many(
        self: Arc<Self>,
        length: u64,
        tasks: u16,
        uris: Arc<[Box<str>]>,
        to: Arc<Path>,
        mut modified: Option<DateTime<Utc>>,
    ) -> Result<(), Error> {
        let parent = to.parent().ok_or(Error::Parentless)?;
        let filename = to.file_name().ok_or(Error::Nameless)?;

        let mut buf = [0u8; 20];

        let mut parts = FuturesOrdered::new();

        // The destination which parts will be concatenated to.
        let concatenated_file =
            &mut File::create(to.as_ref()).await.map_err(Error::FileCreate)?;

        // Create a future for each part to be fetched, and append it to the `parts`
        // stream.
        for task in 0..tasks {
            let uri = uris[task as usize % uris.len()].clone();

            let part_path = {
                let mut new_filename = filename.to_os_string();
                new_filename.push(&[".part", task.numtoa_str(10, &mut buf)].concat());
                parent.join(new_filename)
            };

            let fetcher = self.clone();
            let to = to.clone();

            let future = async move {
                let (offset, offset_to) =
                    range::calc(length, tasks as u64, task as u64).unwrap_or((0, 0));

                let range = range::to_string(offset, offset_to);

                if let Some(sender) = fetcher.events.as_ref() {
                    let _ =
                        sender.unbounded_send(FetchEvent::PartFetching(to.clone(), task));
                }

                let request =
                    fetcher.client.get(&*uri).set_header("range", range.as_str());

                let result = fetcher
                    .get(
                        &mut modified,
                        request,
                        part_path.into(),
                        to.clone(),
                        Some(offset_to - offset),
                    )
                    .await;

                if let Some(sender) = fetcher.events.as_ref() {
                    let _ = sender.unbounded_send(FetchEvent::PartFetched(to, task));
                }

                result
            };

            parts.push(future);
        }

        // Then concatenate those task's files as soon as they are done.
        while let Some(task_result) = parts.next().await {
            let part_path: Arc<Path> = task_result?;
            concatenate(concatenated_file, part_path).await?;
        }

        if let Some(modified) = modified {
            let filetime = FileTime::from_unix_time(modified.timestamp(), 0);
            filetime::set_file_times(&to, filetime, filetime)
                .map_err(|why| Error::FileTime(to, why))?;
        }

        Ok(())
    }
}

async fn concatenate(
    concatenated_file: &mut File,
    part_path: Arc<Path>,
) -> Result<(), Error> {
    {
        let mut file = File::open(&*part_path)
            .await
            .map_err(|why| Error::OpenPart(part_path.clone(), why))?;

        copy(&mut file, concatenated_file).await.map_err(Error::Concatenate)?;
    }

    if let Err(why) = fs::remove_file(&*part_path).await {
        error!("failed to remove part file ({:?}): {}", part_path, why);
    }

    Ok(())
}

fn content_length(headers: &Headers) -> Option<u64> {
    headers.get("content-length").and_then(|header| header.parse::<u64>().ok())
}

fn last_modified(headers: &Headers) -> Option<DateTime<Utc>> {
    headers
        .get("last-modified")
        .and_then(|header| DateTime::parse_from_rfc2822(header).ok())
        .map(|tz| tz.with_timezone(&Utc))
}

async fn head<C: HttpClient>(
    client: &Client<C>,
    uri: &str,
) -> Result<Option<Response>, Error> {
    match validate(client.head(uri).await?).map(Some) {
        result @ Ok(_) => result,
        Err(Error::Status(StatusCode::NOT_IMPLEMENTED)) => Ok(None),
        Err(other) => Err(other),
    }
}

async fn supports_range<C: HttpClient>(
    client: &Client<C>,
    uri: &str,
    length: u64,
) -> Result<bool, Error> {
    let response = client
        .head(uri)
        .set_header("range", range::to_string(0, length).as_str())
        .await?;

    if response.status() == StatusCode::PARTIAL_CONTENT {
        Ok(true)
    } else {
        validate(response).map(|_| false)
    }
}

async fn timed<F, T>(duration: Duration, future: F) -> Result<T, Error>
where
    F: Future<Output = T>,
{
    async_std::future::timeout(duration, future).await.map_err(|_| Error::TimedOut)
}

fn validate(response: Response) -> Result<Response, Error> {
    let status = response.status();

    if status.as_u16() < 300 {
        Ok(response)
    } else {
        Err(Error::Status(status))
    }
}
