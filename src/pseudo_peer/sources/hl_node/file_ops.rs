use super::{scan::Scanner, time_utils::TimeUtils, HOURLY_SUBDIR};
use crate::node::types::BlockAndReceipts;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

pub struct FileOperations;

impl FileOperations {
    pub fn all_hourly_files(root: &Path) -> Option<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(root.join(HOURLY_SUBDIR)).ok()? {
            let dir = entry.ok()?.path();
            if let Ok(subentries) = std::fs::read_dir(&dir) {
                files.extend(
                    subentries
                        .filter_map(|f| f.ok().map(|f| f.path()))
                        .filter_map(|p| TimeUtils::datetime_from_path(&p).map(|dt| (dt, p))),
                );
            }
        }
        files.sort();
        Some(files.into_iter().map(|(_, p)| p).collect())
    }

    pub fn find_latest_hourly_file(root: &Path) -> Option<PathBuf> {
        Self::all_hourly_files(root)?.into_iter().last()
    }

    pub fn read_last_block_from_file(path: &Path) -> Option<(BlockAndReceipts, u64)> {
        let mut file = File::open(path).ok()?;
        Self::read_last_complete_line(&mut file)
    }

    fn read_last_complete_line<R: Read + Seek>(read: &mut R) -> Option<(BlockAndReceipts, u64)> {
        const CHUNK_SIZE: u64 = 50000;
        let mut buf = Vec::with_capacity(CHUNK_SIZE as usize);
        let mut pos = read.seek(SeekFrom::End(0)).unwrap();
        let mut last_line = Vec::new();

        while pos > 0 {
            let read_size = pos.min(CHUNK_SIZE);
            buf.resize(read_size as usize, 0);
            read.seek(SeekFrom::Start(pos - read_size)).unwrap();
            read.read_exact(&mut buf).unwrap();
            last_line = [buf.clone(), last_line].concat();
            if last_line.ends_with(b"\n") {
                last_line.pop();
            }

            if let Some(idx) = last_line.iter().rposition(|&b| b == b'\n') {
                let candidate = &last_line[idx + 1..];
                if let Ok(result) = Scanner::line_to_evm_block(str::from_utf8(candidate).unwrap()) {
                    return Some(result);
                }
                last_line.truncate(idx);
            }
            if pos < read_size {
                break;
            }
            pos -= read_size;
        }
        Scanner::line_to_evm_block(&String::from_utf8(last_line).unwrap()).ok()
    }
}
