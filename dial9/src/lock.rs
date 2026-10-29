// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Exclusive claim on the benchmark machines, mirroring benchy's machine lock: benchmark and
//! profiling runs share the controller host and the peer machine, so only one may use them at
//! a time. Kept inline (rather than depending on benchy-runner) so this workspace builds
//! beside a lone gotatun checkout.

use std::{fs::OpenOptions, path::Path};

use eyre::{Context as _, Result};
use fs2::FileExt;

pub struct MachineLock {
    // The flock lives as long as the file handle.
    _file: std::fs::File,
}

impl MachineLock {
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed to open machine lock {}", path.display()))?;
        file.try_lock_exclusive()
            .with_context(|| format!("benchmark machine is already locked ({})", path.display()))?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_on_the_same_file_is_refused() {
        let path =
            std::env::temp_dir().join(format!("dial9-lock-test-{}.lock", std::process::id()));
        let _first = MachineLock::acquire(&path).expect("first lock");
        assert!(MachineLock::acquire(&path).is_err());
    }
}
