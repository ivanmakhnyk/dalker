//! Where a viewer's file transfers go, and as whom they run.

use std::any::Any;
use std::path::PathBuf;

/// The host user a viewer's file transfers run as: uploads land in `downloads`, downloads come
/// from below `home`.
///
/// It is opened on the thread that does the transfers and acts for that thread only: while it
/// is alive, files are read and created with the user's own permissions (never the agent's,
/// which may be SYSTEM or root). Drop it on the same thread.
pub struct FileAccess {
    /// Account name, for display.
    pub user: String,
    pub home: PathBuf,
    pub downloads: PathBuf,
    /// Keeps the thread acting as `user` until dropped.
    pub guard: Box<dyn Any>,
}

impl std::fmt::Debug for FileAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileAccess")
            .field("user", &self.user)
            .field("home", &self.home)
            .field("downloads", &self.downloads)
            .finish_non_exhaustive()
    }
}
