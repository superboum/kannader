use std::{
    io,
    marker::PhantomData,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use async_trait::async_trait;
use futures::{io::IoSlice, prelude::*};
use openat::Dir;
use smol::unblock;
use smtp_queue::{MailMetadata, QueueId, ScheduleInfo};
use uuid::Uuid;
use walkdir::WalkDir;

pub const DATA_DIR: &str = "data";
pub const QUEUE_DIR: &str = "queue";
pub const INFLIGHT_DIR: &str = "inflight";
pub const CLEANUP_DIR: &str = "cleanup";

pub const DATA_DIR_FROM_OTHER_QUEUE: &str = "../data";

pub const CONTENTS_FILE: &str = "contents";
pub const METADATA_FILE: &str = "metadata";
pub const SCHEDULE_FILE: &str = "schedule";
pub const TMP_METADATA_FILE_PREFIX: &str = "metadata.";
pub const TMP_SCHEDULE_FILE_PREFIX: &str = "schedule.";

const ONLY_USER_RW: u32 = 0o600;
const ONLY_USER_RWX: u32 = 0o700;

// TODO: auto-detect orphan files (pointed to by nowhere in the queue)

#[derive(Debug)]
pub enum QueueType {
    Data,
    Queue,
    Inflight,
    Cleanup,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Opening folder ‘{0}’")]
    OpeningFolder(Arc<PathBuf>, #[source] io::Error),

    #[error("Opening sub-folder ‘{1}’ of folder ‘{0}’")]
    OpeningSubFolder(Arc<PathBuf>, &'static str, #[source] io::Error),

    #[error("Recursively walking directory ‘{0}’")]
    WalkingDirectory(Arc<PathBuf>, #[source] walkdir::Error),

    #[error("Non-UTF-8 path ‘{0}’")]
    NonUtf8Path(Arc<PathBuf>),

    #[error("Opening file ‘{0}’ in folder ‘{1}’")]
    OpeningFileInFolder(PathBuf, Arc<PathBuf>, #[source] io::Error),

    #[error("Parsing JSON from file ‘{0}’")]
    ParsingJson(PathBuf, #[source] serde_json::Error),

    #[error("Opening folder ‘{0}’ in {1:?} queue")]
    OpeningFolderInQueue(Arc<String>, QueueType, #[source] io::Error),

    #[error("Opening file ‘{0}’ in mail ‘{1}’")]
    OpeningFileInMail(&'static str, Arc<String>, #[source] io::Error),

    #[error("Parsing JSON from file ‘{0}’ in mail ‘{1}’ of {2:?} queue")]
    ParsingJsonFileInMail(
        &'static str,
        Arc<String>,
        QueueType,
        #[source] serde_json::Error,
    ),

    #[error("Opening parent directory from mail ‘{0}’")]
    OpeningParentFromMail(Arc<String>, #[source] io::Error),

    #[error("Opening file ‘{0}’ from parent directory of mail ‘{1}’")]
    OpeningFileInMailParent(Arc<String>, #[source] io::Error),

    #[error("Failed making file handle async")]
    MakingFileAsync(#[source] io::Error),

    #[error("Creating folder ‘{0}’ in {1:?} queue")]
    CreatingFolderInQueue(String, QueueType, #[source] io::Error),

    #[error("Creating file ‘{0}’ in folder ‘{1}’ of {2:?} queue")]
    CreatingFileInMail(String, PathBuf, QueueType, #[source] io::Error),

    #[error("Writing JSON into file ‘{0}’ in mail ‘{1}’ of {2:?} queue")]
    WritingJsonFileInMail(String, PathBuf, QueueType, #[source] serde_json::Error),

    #[error("Renaming file from ‘{0}’ to ‘{1}’ in mail ‘{2}’ of {3:?} queue")]
    RenamingFileInMail(
        String,
        &'static str,
        Arc<String>,
        QueueType,
        #[source] io::Error,
    ),

    #[error("Moving mail ‘{0}’ from queue {1:?} to queue {2:?}")]
    MovingMailBetweenQueues(Arc<String>, QueueType, QueueType, #[source] io::Error),

    #[error("Reading link ‘{0}’ in {1:?} queue")]
    ReadingLinkInQueue(Arc<String>, QueueType, #[source] io::Error),

    #[error("Removing file ‘{0}’ from mail ‘{1}’ in {2:?} queue")]
    RemovingFileFromMail(&'static str, PathBuf, QueueType, #[source] io::Error),

    #[error(
        "Mail symlink ‘{0}’ in {1:?} queue points to ‘{2}’ which is not a destination subfolder"
    )]
    SymlinkPointsToNonDestinationSubfolder(Arc<String>, QueueType, PathBuf),

    #[error("Mail symlink ‘{0}’ in {1:?} queue points to ‘{2}’ which is not in the Data queue")]
    SymlinkDoesNotPointToDataQueue(Arc<String>, QueueType, PathBuf),

    #[error("Removing folder ‘{0}’ from {1:?} queue")]
    RemovingFolderFromQueue(PathBuf, QueueType, #[source] io::Error),

    #[error("Listing folder ‘{0}’ in {1:?} queue")]
    ListingFolderInQueue(PathBuf, QueueType, #[source] io::Error),

    #[error("Removing file ‘{0}’ from {1:?} queue")]
    RemovingFileFromQueue(Arc<String>, QueueType, #[source] io::Error),

    #[error("Flushing the changes to file ‘{0}’ of mail ‘{1}’ in {2:?} queue")]
    FlushingMailContents(&'static str, String, QueueType, #[source] io::Error),

    #[error("Creating folder ‘{0}’ in mail ‘{1}’ of {2:?} queue")]
    CreatingFolderInMail(String, String, QueueType, #[source] io::Error),

    #[error("Opening folder ‘{0}’ in mail ‘{1}’ of {2:?} queue")]
    OpeningFolderInMail(String, String, QueueType, #[source] io::Error),

    #[error("Symlinking into file ‘{0}’ of {1:?} queue with destination ‘{2}’")]
    SymlinkingIntoQueue(String, QueueType, PathBuf, #[source] io::Error),
}

pub struct FsStorage<U> {
    path: Arc<PathBuf>,
    data: Arc<Dir>,
    queue: Arc<Dir>,
    inflight: Arc<Dir>,
    cleanup: Arc<Dir>,
    phantom: PhantomData<U>,
}

impl<U> FsStorage<U> {
    pub async fn new(path: Arc<PathBuf>) -> Result<FsStorage<U>, Error> {
        let main_dir = {
            let path2 = path.clone();
            Arc::new(
                unblock(move || Dir::open(&*path2))
                    .await
                    .map_err(|e| Error::OpeningFolder(path.clone(), e))?,
            )
        };
        let data = {
            let main_dir = main_dir.clone();
            Arc::new(
                unblock(move || main_dir.sub_dir(DATA_DIR))
                    .await
                    .map_err(|e| Error::OpeningSubFolder(path.clone(), DATA_DIR, e))?,
            )
        };
        let queue = {
            let main_dir = main_dir.clone();
            Arc::new(
                unblock(move || main_dir.sub_dir(QUEUE_DIR))
                    .await
                    .map_err(|e| Error::OpeningSubFolder(path.clone(), QUEUE_DIR, e))?,
            )
        };
        let inflight = {
            let main_dir = main_dir.clone();
            Arc::new(
                unblock(move || main_dir.sub_dir(INFLIGHT_DIR))
                    .await
                    .map_err(|e| Error::OpeningSubFolder(path.clone(), INFLIGHT_DIR, e))?,
            )
        };
        let cleanup = {
            let main_dir = main_dir.clone();
            Arc::new(
                unblock(move || main_dir.sub_dir(CLEANUP_DIR))
                    .await
                    .map_err(|e| Error::OpeningSubFolder(path.clone(), CLEANUP_DIR, e))?,
            )
        };
        Ok(FsStorage {
            path,
            data,
            queue,
            inflight,
            cleanup,
            phantom: PhantomData,
        })
    }
}

type DynStreamOf<T> = Pin<Box<dyn Send + Stream<Item = T>>>;

#[async_trait]
impl<U> smtp_queue::Storage<U> for FsStorage<U>
where
    U: 'static + Send + Sync + for<'a> serde::Deserialize<'a> + serde::Serialize,
{
    type Enqueuer = FsEnqueuer<U>;
    type Error = Error;
    type InflightLister = DynStreamOf<Result<FsInflightMail, (Error, Option<QueueId>)>>;
    type InflightMail = FsInflightMail;
    type PendingCleanupLister = DynStreamOf<Result<FsPendingCleanupMail, (Error, Option<QueueId>)>>;
    type PendingCleanupMail = FsPendingCleanupMail;
    type QueueLister = DynStreamOf<Result<FsQueuedMail, (Error, Option<QueueId>)>>;
    type QueuedMail = FsQueuedMail;
    type Reader = Pin<Box<dyn Send + AsyncRead>>;

    async fn list_queue(
        &self,
    ) -> Pin<Box<dyn Send + Stream<Item = Result<FsQueuedMail, (Error, Option<QueueId>)>>>> {
        Box::pin(
            scan_queue(self.path.join(QUEUE_DIR), self.queue.clone())
                .await
                .map(|r| r.map(FsQueuedMail::found)),
        )
    }

    async fn find_inflight(
        &self,
    ) -> Pin<Box<dyn Send + Stream<Item = Result<FsInflightMail, (Error, Option<QueueId>)>>>> {
        Box::pin(
            scan_queue(self.path.join(INFLIGHT_DIR), self.inflight.clone())
                .await
                .map(|r| r.map(FsInflightMail::found)),
        )
    }

    async fn find_pending_cleanup(
        &self,
    ) -> Pin<Box<dyn Send + Stream<Item = Result<FsPendingCleanupMail, (Error, Option<QueueId>)>>>>
    {
        Box::pin(
            scan_folder(self.path.join(CLEANUP_DIR))
                .await
                .map(|r| r.map(FsPendingCleanupMail::found)),
        )
    }

    async fn read_inflight(
        &self,
        mail: &FsInflightMail,
    ) -> Result<(MailMetadata<U>, Self::Reader), Error> {
        let inflight = self.inflight.clone();
        let mail = mail.id.0.clone();

        unblock(move || {
            let dest_dir = inflight
                .sub_dir(&*mail)
                .map_err(|e| Error::OpeningFolderInQueue(mail.clone(), QueueType::Inflight, e))?;
            let metadata_file = dest_dir
                .open_file(METADATA_FILE)
                .map_err(|e| Error::OpeningFileInMail(METADATA_FILE, mail.clone(), e))?;
            let metadata = serde_json::from_reader(metadata_file).map_err(|e| {
                Error::ParsingJsonFileInMail(METADATA_FILE, mail.clone(), QueueType::Inflight, e)
            })?;
            let contents_file = dest_dir
                .sub_dir("..")
                .map_err(|e| Error::OpeningParentFromMail(mail.clone(), e))?
                .open_file(CONTENTS_FILE)
                .map_err(|e| Error::OpeningFileInMailParent(mail, e))?;
            let reader =
                Box::pin(smol::Async::new(contents_file).map_err(Error::MakingFileAsync)?) as _;
            Ok((metadata, reader))
        })
        .await
    }

    async fn enqueue(&self) -> Result<FsEnqueuer<U>, Error> {
        let data = self.data.clone();
        let queue = self.queue.clone();

        unblock(move || {
            let mut uuid_buf: [u8; 45] = Uuid::encode_buffer();
            let mail_uuid = Uuid::new_v4()
                .to_hyphenated_ref()
                .encode_lower(&mut uuid_buf);

            data.create_dir(&*mail_uuid, ONLY_USER_RWX).map_err(|e| {
                Error::CreatingFolderInQueue(mail_uuid.to_string(), QueueType::Data, e)
            })?;
            let mail_dir = data.sub_dir(&*mail_uuid).map_err(|e| {
                Error::OpeningFolderInQueue(Arc::new(mail_uuid.to_string()), QueueType::Data, e)
            })?;
            let contents_file = mail_dir
                .new_file(CONTENTS_FILE, ONLY_USER_RW)
                .map_err(|e| {
                    Error::CreatingFileInMail(
                        CONTENTS_FILE.to_string(),
                        PathBuf::from(&*mail_uuid),
                        QueueType::Data,
                        e,
                    )
                })?;

            Ok(FsEnqueuer {
                mail_uuid: mail_uuid.to_string(),
                mail_dir,
                queue,
                writer: Box::pin(smol::Async::new(contents_file).map_err(Error::MakingFileAsync)?),
                phantom: PhantomData,
            })
        })
        .await
    }

    // TODO: make reschedule only ever happen on the inflight mails, as we have an
    // implicit lock on these (note this will require adjusting the QueueType below
    async fn reschedule(
        &self,
        mail: &mut FsQueuedMail,
        schedule: ScheduleInfo,
    ) -> Result<(), Error> {
        mail.schedule = schedule;

        let queue = self.queue.clone();
        let id = mail.id.0.clone();

        unblock(move || {
            let dest_dir = queue
                .sub_dir(&*id)
                .map_err(|e| Error::OpeningFolderInQueue(id.clone(), QueueType::Queue, e))?;

            let mut tmp_sched_file = String::from(TMP_SCHEDULE_FILE_PREFIX);
            let mut uuid_buf: [u8; 45] = Uuid::encode_buffer();
            let uuid = Uuid::new_v4()
                .to_hyphenated_ref()
                .encode_lower(&mut uuid_buf);
            tmp_sched_file.push_str(uuid);

            let tmp_file = dest_dir
                .new_file(&tmp_sched_file, ONLY_USER_RW)
                .map_err(|e| {
                    Error::CreatingFileInMail(
                        tmp_sched_file.to_string(),
                        PathBuf::from(&*id),
                        QueueType::Queue,
                        e,
                    )
                })?;
            serde_json::to_writer(tmp_file, &schedule).map_err(|e| {
                Error::WritingJsonFileInMail(
                    tmp_sched_file.to_string(),
                    PathBuf::from(&*id),
                    QueueType::Queue,
                    e,
                )
            })?;

            dest_dir
                .local_rename(&tmp_sched_file, SCHEDULE_FILE)
                .map_err(|e| {
                    Error::RenamingFileInMail(
                        tmp_sched_file.to_string(),
                        SCHEDULE_FILE,
                        id,
                        QueueType::Queue,
                        e,
                    )
                })?;

            Ok(())
        })
        .await?;
        Ok(())
    }

    // TODO: factor out send_start, send_done, send_cancel, etc.
    async fn send_start(
        &self,
        mail: FsQueuedMail,
    ) -> Result<Option<FsInflightMail>, (FsQueuedMail, Error)> {
        let queue = self.queue.clone();
        let inflight = self.inflight.clone();
        unblock(
            move || match openat::rename(&*queue, &*mail.id.0, &*inflight, &*mail.id.0) {
                Ok(()) => Ok(Some(mail.into_inflight())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => {
                    let id = mail.id.0.clone();
                    Err((
                        mail,
                        Error::MovingMailBetweenQueues(
                            id,
                            QueueType::Queue,
                            QueueType::Inflight,
                            e,
                        ),
                    ))
                }
            },
        )
        .await
    }

    async fn send_done(
        &self,
        mail: FsInflightMail,
    ) -> Result<Option<FsPendingCleanupMail>, (FsInflightMail, Error)> {
        let inflight = self.inflight.clone();
        let cleanup = self.cleanup.clone();
        unblock(
            move || match openat::rename(&*inflight, &*mail.id.0, &*cleanup, &*mail.id.0) {
                Ok(()) => Ok(Some(mail.into_pending_cleanup())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => {
                    let id = mail.id.0.clone();
                    Err((
                        mail,
                        Error::MovingMailBetweenQueues(
                            id,
                            QueueType::Inflight,
                            QueueType::Cleanup,
                            e,
                        ),
                    ))
                }
            },
        )
        .await
    }

    async fn send_cancel(
        &self,
        mail: FsInflightMail,
    ) -> Result<Option<FsQueuedMail>, (FsInflightMail, Error)> {
        let inflight = self.inflight.clone();
        let queue = self.queue.clone();
        unblock(
            move || match openat::rename(&*inflight, &*mail.id.0, &*queue, &*mail.id.0) {
                Ok(()) => Ok(Some(mail.into_queued())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => {
                    let id = mail.id.0.clone();
                    Err((
                        mail,
                        Error::MovingMailBetweenQueues(
                            id,
                            QueueType::Inflight,
                            QueueType::Queue,
                            e,
                        ),
                    ))
                }
            },
        )
        .await
    }

    async fn drop(
        &self,
        mail: FsQueuedMail,
    ) -> Result<Option<FsPendingCleanupMail>, (FsQueuedMail, Error)> {
        let queue = self.queue.clone();
        let cleanup = self.cleanup.clone();
        unblock(
            move || match openat::rename(&*queue, &*mail.id.0, &*cleanup, &*mail.id.0) {
                Ok(()) => Ok(Some(mail.into_pending_cleanup())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => {
                    let id = mail.id.0.clone();
                    Err((
                        mail,
                        Error::MovingMailBetweenQueues(id, QueueType::Queue, QueueType::Cleanup, e),
                    ))
                }
            },
        )
        .await
    }

    async fn cleanup(
        &self,
        mail: FsPendingCleanupMail,
    ) -> Result<bool, (FsPendingCleanupMail, Error)> {
        let cleanup = self.cleanup.clone();
        let data = self.data.clone();
        unblock(move || {
            let dest = match cleanup.read_link(&*mail.id.0) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => {
                    let id = mail.id.0.clone();
                    return Err((mail, Error::ReadingLinkInQueue(id, QueueType::Cleanup, e)));
                }
                Ok(d) => d,
            };

            let dest_dir = match cleanup.sub_dir(&*mail.id.0) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => {
                    let id = mail.id.0.clone();
                    return Err((mail, Error::OpeningFolderInQueue(id, QueueType::Cleanup, e)));
                }
                Ok(d) => d,
            };

            match dest_dir.remove_file(METADATA_FILE) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    let id = PathBuf::from(&*mail.id.0);
                    return Err((
                        mail,
                        Error::RemovingFileFromMail(METADATA_FILE, id, QueueType::Cleanup, e),
                    ));
                }
                _ => (),
            }

            match dest_dir.remove_file(SCHEDULE_FILE) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    let id = PathBuf::from(&*mail.id.0);
                    return Err((
                        mail,
                        Error::RemovingFileFromMail(SCHEDULE_FILE, id, QueueType::Cleanup, e),
                    ));
                }
                _ => (),
            }

            let mail_dir = match dest_dir.sub_dir("..") {
                Ok(d) => d,
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    let id = mail.id.0.clone();
                    return Err((mail, Error::OpeningParentFromMail(id, e)));
                }
                Err(_) => return Ok(false),
            };

            let dest_name = match dest.file_name() {
                Some(d) => d,
                None => {
                    let id = mail.id.0.clone();
                    return Err((
                        mail,
                        Error::SymlinkPointsToNonDestinationSubfolder(id, QueueType::Cleanup, dest),
                    ));
                }
            };

            match mail_dir.remove_dir(dest_name) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    let id = PathBuf::from(&*mail.id.0);
                    return Err((
                        mail,
                        Error::RemovingFolderFromQueue(id, QueueType::Cleanup, e),
                    ));
                }
                _ => (),
            }

            // rm mail_dir iff the only remaining file is CONTENTS_FILE
            // `mut` is required here because list_self() returns an Iterator
            let mut mail_dir_list = match mail_dir.list_self() {
                Ok(l) => l,
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    let mail_dir_path = dest
                        .parent()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| dest.join(".."));
                    return Err((
                        mail,
                        Error::ListingFolderInQueue(mail_dir_path, QueueType::Cleanup, e),
                    ));
                }
                Err(_) => return Ok(false),
            };
            let should_rm_mail_dir =
                mail_dir_list.all(|e| matches!(e, Ok(e) if e.file_name() == CONTENTS_FILE));

            if should_rm_mail_dir {
                match mail_dir.remove_file(CONTENTS_FILE) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => {
                        let mail_dir_path = dest
                            .parent()
                            .map(PathBuf::from)
                            .unwrap_or_else(|| dest.join(".."));
                        return Err((
                            mail,
                            Error::RemovingFileFromMail(
                                CONTENTS_FILE,
                                mail_dir_path,
                                QueueType::Cleanup,
                                e,
                            ),
                        ));
                    }
                    _ => (),
                }

                let mail_name = match dest.strip_prefix(DATA_DIR_FROM_OTHER_QUEUE) {
                    Ok(m) => match m.parent() {
                        Some(m) => m,
                        None => {
                            let id = mail.id.0.clone();
                            return Err((
                                mail,
                                Error::SymlinkPointsToNonDestinationSubfolder(
                                    id,
                                    QueueType::Cleanup,
                                    dest,
                                ),
                            ));
                        }
                    },
                    Err(_) => {
                        // StripPrefixError contains no useful information
                        let id = mail.id.0.clone();
                        return Err((
                            mail,
                            Error::SymlinkDoesNotPointToDataQueue(id, QueueType::Cleanup, dest),
                        ));
                    }
                };

                match data.remove_dir(mail_name) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => {
                        return Err((
                            mail,
                            Error::RemovingFolderFromQueue(
                                mail_name.to_owned(),
                                QueueType::Data,
                                e,
                            ),
                        ));
                    }
                    _ => (),
                }
            }

            match cleanup.remove_file(&*mail.id.0) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => {
                    let id = mail.id.0.clone();
                    Err((
                        mail,
                        Error::RemovingFileFromQueue(id, QueueType::Cleanup, e),
                    ))
                }
            }
        })
        .await
    }
}

struct FoundMail {
    id: QueueId,
    schedule: ScheduleInfo,
}

async fn scan_folder<P>(
    path: P,
) -> impl 'static + Send + Stream<Item = Result<QueueId, (Error, Option<QueueId>)>>
where
    P: 'static + Send + AsRef<Path>,
{
    let root_path = Arc::new(path.as_ref().to_owned());
    let it = unblock(move || WalkDir::new(path).into_iter()).await;
    smol::stream::iter(it)
        .then(move |p| {
            let root_path = root_path.clone();
            async move {
                let p = p.map_err(|e| (Error::WalkingDirectory(root_path.clone(), e), None))?;
                if !p.path_is_symlink() {
                    Ok(None)
                } else {
                    let path_str = p
                        .path()
                        .to_str()
                        .ok_or_else(|| (Error::NonUtf8Path(root_path.clone()), None))?;
                    Ok(Some(QueueId::new(path_str)))
                }
            }
        })
        .filter_map(|r| async move { r.transpose() })
}

async fn scan_queue<P>(
    path: P,
    dir: Arc<Dir>,
) -> impl 'static + Send + Stream<Item = Result<FoundMail, (Error, Option<QueueId>)>>
where
    P: 'static + Send + AsRef<Path>,
{
    let root_path = Arc::new(path.as_ref().to_owned());
    scan_folder(path).await.then(move |id| {
        let dir = dir.clone();
        let root_path = root_path.clone();
        async move {
            let id = id?;
            let schedule_path = Path::new(&*id.0).join(SCHEDULE_FILE);
            let schedule = unblock(move || {
                let schedule_file = dir.open_file(&schedule_path).map_err(|e| {
                    Error::OpeningFileInFolder(schedule_path.clone(), root_path.clone(), e)
                })?;
                serde_json::from_reader(schedule_file).map_err(|e| {
                    let file_path = root_path.join(schedule_path);
                    Error::ParsingJson(file_path, e)
                })
            })
            .await
            .map_err(|e| (e, Some(id.clone())))?;
            Ok(FoundMail { id, schedule })
        }
    })
}

pub struct FsQueuedMail {
    id: QueueId,
    schedule: ScheduleInfo,
}

impl FsQueuedMail {
    fn found(f: FoundMail) -> FsQueuedMail {
        FsQueuedMail {
            id: f.id,
            schedule: f.schedule,
        }
    }

    fn into_inflight(self) -> FsInflightMail {
        FsInflightMail {
            id: self.id,
            schedule: self.schedule,
        }
    }

    fn into_pending_cleanup(self) -> FsPendingCleanupMail {
        FsPendingCleanupMail { id: self.id }
    }
}

impl smtp_queue::QueuedMail for FsQueuedMail {
    fn id(&self) -> QueueId {
        self.id.clone()
    }

    fn schedule(&self) -> ScheduleInfo {
        self.schedule
    }
}

pub struct FsInflightMail {
    id: QueueId,
    schedule: ScheduleInfo,
}

impl FsInflightMail {
    fn found(f: FoundMail) -> FsInflightMail {
        FsInflightMail {
            id: f.id,
            schedule: f.schedule,
        }
    }

    fn into_queued(self) -> FsQueuedMail {
        FsQueuedMail {
            id: self.id,
            schedule: self.schedule,
        }
    }

    fn into_pending_cleanup(self) -> FsPendingCleanupMail {
        FsPendingCleanupMail { id: self.id }
    }
}

impl smtp_queue::InflightMail for FsInflightMail {
    fn id(&self) -> QueueId {
        self.id.clone()
    }
}

pub struct FsPendingCleanupMail {
    id: QueueId,
}

impl FsPendingCleanupMail {
    fn found(id: QueueId) -> FsPendingCleanupMail {
        FsPendingCleanupMail { id }
    }
}

impl smtp_queue::PendingCleanupMail for FsPendingCleanupMail {
    fn id(&self) -> QueueId {
        self.id.clone()
    }
}

pub struct FsEnqueuer<U> {
    mail_uuid: String,
    mail_dir: Dir,
    queue: Arc<Dir>,
    writer: Pin<Box<dyn 'static + Send + AsyncWrite>>,
    // FsEnqueuer needs the U type parameter just so as to be able to take it as a parameter later
    // on
    phantom: PhantomData<fn(U)>,
}

/// Blocking function!
fn make_dest_dir<U>(
    queue: &Dir,
    mail_uuid: &str,
    mail_dir: &Dir,
    dest_id: &str,
    metadata: &MailMetadata<U>,
    schedule: &ScheduleInfo,
) -> Result<FsQueuedMail, Error>
where
    U: 'static + Send + Sync + for<'a> serde::Deserialize<'a> + serde::Serialize,
{
    // TODO: clean up self dest dir when having an io error
    mail_dir.create_dir(dest_id, ONLY_USER_RWX).map_err(|e| {
        Error::CreatingFolderInMail(
            dest_id.to_string(),
            mail_uuid.to_string(),
            QueueType::Data,
            e,
        )
    })?;
    let dest_dir = mail_dir.sub_dir(dest_id).map_err(|e| {
        Error::OpeningFolderInMail(
            dest_id.to_string(),
            mail_uuid.to_string(),
            QueueType::Data,
            e,
        )
    })?;

    let dest_path = || Path::new(mail_uuid).join(dest_id);

    let schedule_file = dest_dir
        .new_file(SCHEDULE_FILE, ONLY_USER_RW)
        .map_err(|e| {
            Error::CreatingFileInMail(SCHEDULE_FILE.to_string(), dest_path(), QueueType::Data, e)
        })?;
    serde_json::to_writer(schedule_file, &schedule).map_err(|e| {
        Error::WritingJsonFileInMail(SCHEDULE_FILE.to_string(), dest_path(), QueueType::Data, e)
    })?;

    let metadata_file = dest_dir
        .new_file(METADATA_FILE, ONLY_USER_RW)
        .map_err(|e| {
            Error::CreatingFileInMail(METADATA_FILE.to_string(), dest_path(), QueueType::Data, e)
        })?;
    serde_json::to_writer(metadata_file, &metadata).map_err(|e| {
        Error::WritingJsonFileInMail(METADATA_FILE.to_string(), dest_path(), QueueType::Data, e)
    })?;

    let mut dest_uuid_buf: [u8; 45] = Uuid::encode_buffer();
    let dest_uuid = Uuid::new_v4()
        .to_hyphenated_ref()
        .encode_lower(&mut dest_uuid_buf);

    let mut symlink_value = PathBuf::from(DATA_DIR_FROM_OTHER_QUEUE);
    symlink_value.push(&mail_uuid);
    symlink_value.push(dest_id);
    queue.symlink(&*dest_uuid, &symlink_value).map_err(|e| {
        Error::SymlinkingIntoQueue(dest_uuid.to_string(), QueueType::Queue, symlink_value, e)
    })?;

    Ok(FsQueuedMail::found(FoundMail {
        id: QueueId(Arc::new(dest_uuid.to_string())),
        schedule: *schedule,
    }))
}

/// Blocking function!
fn cleanup_dest_dir(mail_dir: &Dir, dest_id: &str) {
    // TODO: consider logging IO errors on cleanups that follow an IO error
    let dest_dir = match mail_dir.sub_dir(dest_id) {
        Ok(d) => d,
        Err(_) => return,
    };
    let _ = dest_dir.remove_file(METADATA_FILE);
    let _ = dest_dir.remove_file(SCHEDULE_FILE);

    let _ = mail_dir.remove_dir(dest_id);
}

/// Blocking function!
fn cleanup_contents_dir(queue: &Dir, mail_uuid: String, mail_dir: &Dir) {
    // TODO: consider logging IO errors on cleanups that follow an IO error
    let _ = mail_dir.remove_file(CONTENTS_FILE);
    let _ = queue.remove_dir(mail_uuid);
}

#[async_trait]
impl<U> smtp_queue::StorageEnqueuer<U, FsStorage<U>, FsQueuedMail> for FsEnqueuer<U>
where
    U: 'static + Send + Sync + for<'a> serde::Deserialize<'a> + serde::Serialize,
{
    async fn commit(
        mut self,
        destinations: Vec<(MailMetadata<U>, ScheduleInfo)>,
    ) -> Result<Vec<FsQueuedMail>, Error> {
        match self.flush().await {
            Ok(()) => (),
            Err(e) => {
                let mail_uuid = self.mail_uuid.clone();
                unblock(move || cleanup_contents_dir(&self.queue, self.mail_uuid, &self.mail_dir))
                    .await;
                return Err(Error::FlushingMailContents(
                    CONTENTS_FILE,
                    mail_uuid,
                    QueueType::Data,
                    e,
                ));
            }
        }
        let destinations = destinations
            .into_iter()
            .map(|(meta, sched)| {
                let mut uuid_buf: [u8; 45] = Uuid::encode_buffer();
                let dest_uuid = Uuid::new_v4()
                    .to_hyphenated_ref()
                    .encode_lower(&mut uuid_buf);
                (dest_uuid.to_string(), meta, sched)
            })
            .collect::<Vec<_>>();
        unblock(move || {
            let mut queued_mails = Vec::with_capacity(destinations.len());

            for d in 0..destinations.len() {
                match make_dest_dir(
                    &self.queue,
                    &self.mail_uuid,
                    &self.mail_dir,
                    &destinations[d].0,
                    &destinations[d].1,
                    &destinations[d].2,
                ) {
                    Ok(queued_mail) => queued_mails.push(queued_mail),
                    Err(e) => {
                        for dest in &destinations[0..d] {
                            cleanup_dest_dir(&self.mail_dir, &dest.0);
                        }
                        cleanup_contents_dir(&self.queue, self.mail_uuid, &self.mail_dir);
                        return Err(e);
                    }
                }
            }

            Ok(queued_mails)
        })
        .await
    }
}

impl<U> AsyncWrite for FsEnqueuer<U> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.writer) }.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.writer) }.poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.writer) }.poll_close(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context,
        bufs: &[IoSlice],
    ) -> Poll<io::Result<usize>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.writer) }.poll_write_vectored(cx, bufs)
    }
}
