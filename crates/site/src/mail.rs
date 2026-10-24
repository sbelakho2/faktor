use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::{SiteError, SiteResult};

const MAX_BODY_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub struct MailMessage {
    pub to: String,
    pub subject: String,
    pub body: String,
}

impl fmt::Debug for MailMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailMessage")
            .field("to", &self.to)
            .field("subject", &self.subject)
            .field("body", &"<redacted>")
            .finish()
    }
}

pub trait Mailer: Send + Sync {
    fn send(&self, message: &MailMessage) -> SiteResult<()>;
}

pub struct FileMailer {
    directory: PathBuf,
}

impl FileMailer {
    pub fn new(directory: PathBuf) -> SiteResult<Self> {
        std::fs::create_dir_all(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { directory })
    }
}

impl Mailer for FileMailer {
    fn send(&self, message: &MailMessage) -> SiteResult<()> {
        if message.to.len() > 320
            || message.subject.len() > 200
            || message.body.is_empty()
            || message.body.len() > MAX_BODY_BYTES
        {
            return Err(SiteError::internal_message(
                "outbound mail exceeded its bounds",
            ));
        }
        let path = self.directory.join(format!("{}.eml", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        write!(
            file,
            "To: {}\nSubject: {}\nContent-Type: text/plain; charset=utf-8\n\n{}\n",
            message.to, message.subject, message.body
        )?;
        file.sync_all()?;
        tracing::info!(recipient = %message.to, file = %path.display(), "local mail delivered");
        Ok(())
    }
}

#[derive(Default)]
pub struct MemoryMailer {
    messages: Mutex<Vec<MailMessage>>,
}

impl MemoryMailer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn messages(&self) -> Vec<MailMessage> {
        self.messages
            .lock()
            .map(|messages| messages.clone())
            .unwrap_or_default()
    }

    pub fn last(&self) -> Option<MailMessage> {
        self.messages
            .lock()
            .ok()
            .and_then(|messages| messages.last().cloned())
    }
}

impl Mailer for MemoryMailer {
    fn send(&self, message: &MailMessage) -> SiteResult<()> {
        let mut messages = self
            .messages
            .lock()
            .map_err(|_| SiteError::internal_message("mail capture lock is poisoned"))?;
        if messages.len() >= 64 {
            messages.remove(0);
        }
        messages.push(message.clone());
        Ok(())
    }
}
