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

use std::collections::HashMap;

use tokio::{fs::{File, OpenOptions}, io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt}, sync::{Mutex, RwLock}};


pub struct FileManager {
    files: RwLock<HashMap<String, Mutex<File>>>
}

impl FileManager {
    pub fn new() -> FileManager {
        FileManager {
            files: RwLock::new(HashMap::new())
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

        files.insert(file_path.to_string(), Mutex::new(file));
        Ok(())
    }

    pub async fn read_at(&self, file_path: &str, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let mut files = self.files.read().await;
        let mut file = files.get(file_path);

        if let None = file {
            self.open_file(file_path).await?;
            files = self.files.read().await;
            file = files.get(file_path);
        }

        let mut file = file.unwrap().lock().await;

        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.read(buf).await?;
        Ok(())
    }

    pub async fn append(&self, file_path: &str, buf: &[u8]) -> anyhow::Result<u64> {
        let mut files = self.files.read().await;
        let mut file = files.get(file_path);

        if let None = file {
            self.open_file(file_path).await?;
            files = self.files.read().await;
            file = files.get(file_path);
        }

        let mut file = file.unwrap().lock().await;
        file.seek(std::io::SeekFrom::End(0)).await?;
        let ret = file.stream_position().await?;
        file.write_all(buf).await?;

        Ok(ret)
    }
}