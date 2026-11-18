//! Every fixed limit of the server, in one place. The code enforces these
//! values, the API description lists them (`x-nisaba-limits`), and the tests
//! check them at their edges. Limits an administrator can change are
//! settings ([`crate::config`]).

use std::time::Duration;

/// The largest JSON body of most routes.
pub const MAX_BODY: usize = 64 * 1024;
/// The largest asset upload.
pub const MAX_ASSET: usize = 32 << 20;
/// The largest project import (`POST /api/projects` with files).
pub const MAX_IMPORT: usize = 64 << 20;

/// Sync sockets one account may hold open at once.
pub const SOCKETS_PER_ACCOUNT: usize = 32;

/// Password hashes computed at once. Each takes about 19 MiB and a core.
pub const PASSWORD_WORKERS: usize = 4;
/// Password requests that may wait for a worker.
pub const PASSWORD_QUEUE: usize = 16;
/// The shortest password.
pub const PASSWORD_MIN: usize = 8;
/// The longest password, in bytes. Argon2 takes any length; this keeps a
/// request from making the server hash megabytes.
pub const PASSWORD_MAX: usize = 1024;

/// The longest display name, in characters.
pub const NAME_MAX: usize = 100;
/// The longest project name, in characters.
pub const PROJECT_NAME_MAX: usize = 200;
/// The longest email, in bytes.
pub const EMAIL_MAX: usize = 254;
/// The longest file path in a project, in bytes.
pub const PATH_MAX: usize = 512;

/// A session ends this long after it was last renewed...
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
/// ...it is renewed at most this often...
pub const SESSION_RENEW_EVERY: Duration = Duration::from_secs(24 * 3600);
/// ...and never lasts beyond this after sign-in.
pub const SESSION_MAX_AGE: Duration = Duration::from_secs(90 * 24 * 3600);

/// The largest package archive the server downloads or stores.
pub const PACKAGE_MAX: usize = 16 << 20;
/// The most bytes a package may unpack to.
pub const PACKAGE_UNPACKED_MAX: u64 = 64 << 20;
/// The most files a package may hold.
pub const PACKAGE_FILES_MAX: usize = 4096;
/// The most packages one export loads, its imports' imports included.
pub const PACKAGES_PER_EXPORT: usize = 64;
/// How long a package download may take.
pub const PACKAGE_DOWNLOAD: Duration = Duration::from_secs(30);

/// How long a stopping server waits for open requests and sockets.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The limits of one project room.
#[must_use]
pub fn room() -> nisaba_sync::Limits {
    nisaba_sync::Limits::default()
}
