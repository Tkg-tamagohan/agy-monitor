use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags};

use crate::{CoreError, Result};

pub const BUSY_TIMEOUT_MS: u64 = 150;
pub const RETRY_BACKOFFS_MS: [u64; 3] = [30, 80, 160];

pub fn open_readonly(path: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    conn.execute_batch("PRAGMA query_only = ON;")?;
    Ok(conn)
}

pub fn is_busy_or_locked(err: &CoreError) -> bool {
    matches!(
        err,
        CoreError::Sqlite(rusqlite::Error::SqliteFailure(ffi_err, _))
            if ffi_err.code == ErrorCode::DatabaseBusy
                || ffi_err.code == ErrorCode::DatabaseLocked
    )
}

pub fn with_readonly_retry<T, F>(path: &Path, mut f: F) -> Result<T>
where
    F: FnMut(&Connection) -> Result<T>,
{
    with_readonly_retry_schedule(path, &RETRY_BACKOFFS_MS, |conn| f(conn))
}

pub(crate) fn with_readonly_retry_schedule<T, F>(
    path: &Path,
    backoffs_ms: &[u64],
    mut f: F,
) -> Result<T>
where
    F: FnMut(&Connection) -> Result<T>,
{
    for attempt in 0..=backoffs_ms.len() {
        let res = match open_readonly(path) {
            Ok(conn) => f(&conn),
            Err(e) => Err(e),
        };
        match res {
            Ok(val) => return Ok(val),
            Err(err) if is_busy_or_locked(&err) => {
                if attempt < backoffs_ms.len() {
                    std::thread::sleep(Duration::from_millis(backoffs_ms[attempt]));
                } else {
                    return Err(CoreError::DbBusy {
                        path: path.display().to_string(),
                    });
                }
            }
            Err(err) => return Err(err),
        }
    }
    Err(CoreError::DbBusy {
        path: path.display().to_string(),
    })
}
