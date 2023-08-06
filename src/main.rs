use gofile_api::{
    Api,
    Content,
    ContentKind,
};

use clap::{
    Parser,
    Subcommand,
};

use url::{
    Url,
};

use uuid::{
    Uuid,
};

use std::{
    error,
    io,
    pin::{
        Pin,
    },
    env::{
        self,
        VarError,
    },
    fmt::{
        self,
        Display,
        Formatter,
    },
    path::{
        PathBuf,
    },
    task::{
        Poll,
        Context,
    },
};

use futures::{
    TryStreamExt,
    io::{
        AsyncWrite,
    },
};

use tokio::{
    fs::{
        File,
        metadata,
    },
};

use tokio_util::{
    compat::{
        TokioAsyncReadCompatExt,
    },
};

use md5;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Download {
        #[arg(value_parser = ContentId::parse_content_id)]
        content_id: ContentId,
    },
    Upload {
        #[arg()]
        path: PathBuf,

        #[arg(long)]
        public: bool,
    },
}

#[derive(Debug)]
enum Error {
    TokenNotPresent,
    TokenNotUnicode,
    InvalidUrl(Url),
    InvalidContentUrl(Url),
    InvalidDownloadUrl(Url),
    InvalidTopLevelFile(String),
    NoContent,
    NotImplementedForSubdir,
    HttpRequestError(reqwest::Error),
    GoFileApiError(gofile_api::Error),
    FileCouldntBeCreated(String),
    FileCouldntBeWritten(String),
    CouldntReadMetadata(String),
    NotAFile(PathBuf),
    Md5DigestMismatched(String),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl error::Error for Error {
}

impl From<VarError> for Error {
    fn from(err: VarError) -> Self {
        match err {
            VarError::NotPresent => Self::TokenNotPresent,
            VarError::NotUnicode(_) => Self::TokenNotUnicode,
        }
    }
}

impl From<gofile_api::Error> for Error {
    fn from(err: gofile_api::Error) -> Self {
        Self::GoFileApiError(err)
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Self::HttpRequestError(err)
    }
}

#[derive(Clone, Debug)]
enum ContentId {
    DownloadUrl(Url, String),
    Uuid(Uuid),
    Code(String),
}

impl ContentId {
    fn parse_content_id(content_id_str: &str) -> Result<ContentId, Error> {
        if let Ok(uuid) = Uuid::parse_str(content_id_str) {
            return Ok(ContentId::Uuid(uuid));
        };
        if let Ok(url) = Url::parse(content_id_str) {
            let Some(mut segs) = url.path_segments() else {
                return Err(Error::InvalidUrl(url));
            };
            match segs.next() {
                Some("d") => {
                    let Some(code) = segs.next() else {
                        return Err(Error::InvalidContentUrl(url));
                    };

                    if let Some(_) = segs.next() {
                        return Err(Error::InvalidContentUrl(url));
                    };

                    return Ok(ContentId::Code(String::from(code)))
                },
                Some("download") => {
                    let Some(uuid_str) = segs.next() else {
                        return Err(Error::InvalidDownloadUrl(url));
                    };
                    let Ok(_) = Uuid::parse_str(uuid_str) else {
                        return Err(Error::InvalidDownloadUrl(url));
                    };
                    let Some(filename) = segs.next() else {
                        return Err(Error::InvalidDownloadUrl(url));
                    };

                    if let Some(_) = segs.next() {
                        return Err(Error::InvalidDownloadUrl(url));
                    };

                    return Ok(ContentId::DownloadUrl(url.clone(), String::from(filename)));
                    
                },
                _ => return Err(Error::InvalidUrl(url)),
            };
        };
        Ok(ContentId::Code(String::from(content_id_str)))
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args = Cli::parse();
    match args.command {
        Command::Download { content_id } => {
            download(content_id).await
        },
        Command::Upload { path, public } => {
            upload(path, public).await
        },
    }
}

async fn download(content_id: ContentId) -> Result<(), Error> {
    let api = Api::new();
    let token = get_token()?;
    let api = api.authorize(&token);
    match content_id {
        ContentId::DownloadUrl(url, filename) => {
            let _ = download_impl(url, filename, &token).await?;
            Ok(())
        },
        ContentId::Uuid(id) => {
            let content = api.get_content_by_id(id).await?;
            download_all_child_contents(content, &token).await
        },
        ContentId::Code(code) => {
            let content = api.get_content_by_code(code).await?;
            download_all_child_contents(content, &token).await
        },
    }
}

async fn upload(path: PathBuf, public: bool) -> Result<(), Error> {
    let api = Api::new();
    let token = get_token()?;
    let api = api.authorize(&token);

    let metadata = match metadata(&path).await {
        Ok(metadata) => metadata,
        Err(err) => return Err(Error::CouldntReadMetadata(format!("{}", err))),
    };

    if !metadata.is_file() {
        return Err(Error::NotAFile(path))
    };

    let server_api = api.get_server().await?;
    let uploaded_file_info = server_api.upload_file(path).await?;

    let content_id = uploaded_file_info.parent_folder;

    if public {
        api.set_public_option(content_id, true).await?;
    } else {
        api.set_public_option(content_id, false).await?;
    }

    println!("{}", uploaded_file_info.download_page);

    Ok(())
}

async fn download_impl(url: Url, filename: String, token: &str) -> Result<md5::Digest, Error> {
    let client = reqwest::Client::new();
    let res = client.get(url)
        .header("Cookie", format!("accountToken={}", token))
        .send()
        .await?;
    let mut byte_stream = res.bytes_stream().map_err(|err| io::Error::new(io::ErrorKind::Other, err)).into_async_read();
    let file = match File::create(filename).await {
        Ok(file) => file.compat(),
        Err(err) => return Err(Error::FileCouldntBeCreated(format!("{}", err))),
    };
    let mut file = Md5Filter::new(file);
    match futures::io::copy(&mut byte_stream, &mut file).await {
        Err(err) => Err(Error::FileCouldntBeWritten(format!("{}", err))),
        Ok(_) => Ok(file.compute_digest()),
    }
}

async fn download_all_child_contents(content: Content, token: &str) -> Result<(), Error> {
    let ContentKind::Folder { contents, .. } = content.kind else {
        return Err(Error::InvalidTopLevelFile(content.name));
    };
    let Some(contents) = contents else {
        return Err(Error::NoContent);
    };
    if 0 == contents.len() {
        return Err(Error::NoContent);
    };

    for (_, content) in contents {
        let ContentKind::File { link, md5, .. } = content.kind else {
            return Err(Error::NotImplementedForSubdir);
        };
        let digest = download_impl(link, content.name, token).await?;
        if md5 != digest.0 {
            return Err(Error::Md5DigestMismatched(format!("{:x} != {:x}", md5::Digest(md5), digest)));
        };
    };
    Ok(())
}

fn get_token() -> Result<String, Error> {
    Ok(env::var("GOFILE_TOKEN")?)
}


struct Md5Filter<W: AsyncWrite> {
    writer: W,
    md5_cx: md5::Context,
}

impl<W: AsyncWrite> Md5Filter<W> {
    fn new(writer: W) -> Self {
        Self { writer, md5_cx: md5::Context::new() }
    }

    fn compute_digest(self) -> md5::Digest {
        self.md5_cx.compute()
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Md5Filter<W> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.writer).poll_write(cx, buf) {
            Poll::Ready(Ok(size)) => {
                self.md5_cx.consume(&buf[..size]);
                Poll::Ready(Ok(size))
            },
            other => other,
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_close(cx)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(cx)
    }
}

