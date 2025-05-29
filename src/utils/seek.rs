/**
 *  This file is part of Masstuffy. Masstuffy is free software:
 *  you can redistribute it and/or modify it under the terms of 
 *  the GNU Affero General Public License as published by
 *  the Free Software Foundation, either version 3 of the License,
 *  or (at your option) any later version.
 * 
 *  Masstuffy is distributed in the hope that it will be useful,
 *  but WITHOUT ANY WARRANTY; without even the implied warranty of
 *  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
 * 
 *  See the GNU Affero General Public License for more details.
 *  You should have received a copy of the GNU Affero General Public License
 *  along with Masstuffy. If not, see <https://www.gnu.org/licenses/>. 
 * 
 *  Copyright (C) 2025 5IGI0 / Ethan L. C. Lorenzetti
**/

use std::{collections::HashMap, os::unix::fs::MetadataExt, sync::Arc};

use tokio::{fs::{File, OpenOptions}, io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader}, sync::{Mutex, RwLock}};

pub struct FileManager {
    files: RwLock<HashMap<String, Arc<Mutex<BufReader<File>>>>>,
    filesizes: RwLock<HashMap<String, Option<u64>>>
}

impl FileManager {
    pub fn new() -> FileManager {
        FileManager {
            files: RwLock::new(HashMap::new()),
            filesizes: RwLock::new(HashMap::new())
        }
    }

    async fn open_file(&self, file_path: &str) -> anyhow::Result<()> {
        let mut files = self.files.write().await;

        if let Some(_) = files.get(file_path) {
            return Ok(())
        }

        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(file_path)
            .await?;

        files.insert(file_path.to_string(), Arc::new(Mutex::new(BufReader::new(file))));
        Ok(())
    }

    pub async fn read_at(&self, file_path: &str, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let mut files = self.files.read().await;
        let mut file = files.get(file_path);

        if let None = file {
            drop(files);
            self.open_file(file_path).await?;
            files = self.files.read().await;
            file = files.get(file_path);
        }

        let mut file = file.unwrap().lock().await;

        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.read(buf).await?;
        Ok(())
    }

    pub async fn get_file(&self, file_path: &str) -> anyhow::Result<Arc<Mutex<BufReader<File>>>> {
        let mut files = self.files.read().await;
        let mut file = files.get(file_path);

        if let None = file {
            drop(files);
            self.open_file(file_path).await?;
            files = self.files.read().await;
            file = files.get(file_path);
        }

        return Ok(file.unwrap().clone());
    }

    // when a file is deleed, we have to "unmanage it" or we will keep an alive fp on it.
    pub async fn unmanage_file(&self, file_path: &str) {
        let mut files = self.files.write().await;

        files.remove(file_path);
    }

    pub async fn append(&self, file_path: &str, buf: &[u8]) -> anyhow::Result<u64> {
        let mut files = self.files.read().await;
        let mut file = files.get(file_path);

        if let None = file {
            drop(files);
            self.open_file(file_path).await?;
            files = self.files.read().await;
            file = files.get(file_path);
        }

        let mut file = file.unwrap().lock().await;
        file.seek(std::io::SeekFrom::End(0)).await?;
        let ret = file.stream_position().await?;
        file.write_all(buf).await?;
        file.flush().await?;

        self.filesizes.write().await.insert(file_path.to_string(), Some(ret+buf.len() as u64));

        Ok(ret)
    }

    pub async fn get_file_size(&self, file_path: String) -> Option<u64> {
        let filesize = self.filesizes.read().await.get(&file_path).cloned();

        if let Some(filesize) = filesize {
            filesize
        } else {
            let ret = tokio::fs::metadata(&file_path).await.ok().map(|f| f.size());
            self.filesizes.write().await.insert(file_path, ret);
            ret
        }
    }
}