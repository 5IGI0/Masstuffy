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
 *  See the GNU General Public License for more details.
 *  You should have received a copy of the GNU General Public License
 *  along with Masstuffy. If not, see <https://www.gnu.org/licenses/>. 
 * 
 *  Copyright (C) 2025 5IGI0 / Ethan L. C. Lorenzetti
**/

use std::fmt;

use anyhow::bail;
use log::{debug, error};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

use crate::utils::open_compressed;

use super::WarcRecord;


// TODO: "massaged url"
pub struct CDXRecord {
    url: Option<String>,
    record_type: String,
    record_id: String,
    date: String,
    file_name: Option<String>,
    file_offset: Option<String>
}

fn part2option(part: &str) -> Option<String> {
    if part == "-" {
        None
    } else {
        Some(part.to_string())
    }
}

impl CDXRecord {
    pub fn from_warc(warc: &WarcRecord) -> anyhow::Result<Self> {
        Ok(CDXRecord{
            url: warc.get_target_uri(),
            record_type: warc.get_type()?,
            record_id: warc.get_record_id()?,
            date: warc.get_date()?.format("%Y%m%d%H%M%S").to_string(),
            file_name: None,
            file_offset: None
        })
    }

    pub fn from_line(line: &str) -> anyhow::Result<Self> {
        let parts: Vec<&str> = line.split(' ').collect();

        if parts.len() != 6 && (parts.len() != 7 || parts[6] != "\n"){
            bail!("expected 6 parts but found {}", parts.len());
        }

        Ok(CDXRecord{
            url: part2option(parts[0]),
            record_type: parts[1].to_string(),
            record_id: parts[2].to_string(),
            date: parts[3].to_string(),
            file_name: part2option(parts[4]),
            file_offset: part2option(parts[5].trim())
        })
    }

    pub fn set_file(&mut self, filename: String, offset: Option<u64>) {
        self.file_name = Some(filename);
        if let Some(x) = offset{
            self.file_offset = Some(format!("{}", x));
        } else {
            self.file_offset = None
        }
    }

    pub fn get_date(&self) -> String {self.date.clone()}
    pub fn get_record_id(&self) -> String {self.record_id.clone()}
    pub fn get_record_type(&self) -> String {self.record_type.clone()}
    pub fn get_url(&self) -> Option<String> {self.url.clone()}
    pub fn get_file_name(&self) -> Option<String> {self.file_name.clone()}
    pub fn get_file_offset(&self) -> Option<i64> {
        if let Some(x) = &self.file_offset {
            if let Ok(b) = x.parse::<i64>() {
                Some(b)
            } else {
                None
            }
        } else {
            None
        }
    }
}

impl fmt::Display for CDXRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f,
            "{} {} {} {} {} {}",
            self.url.clone().unwrap_or("-".to_string()), self.record_type,
            self.record_id, self.date,
            self.file_name.clone().unwrap_or("-".to_string()),
            self.file_offset.clone().unwrap_or("-".to_string())
        )?;
        Ok(())
    }
}

pub struct CDXFileReader {
    br: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    buff: String
}

impl CDXFileReader {
    pub async fn open(path: &str) -> anyhow::Result<Self> {
        Ok(CDXFileReader{
            br: open_compressed(path).await?,
            buff: String::new()
        })
    }

    pub async fn async_next(&mut self) -> Option<CDXRecord> {
        self.buff.clear();
        if let Err(x) = self.br.read_line(&mut self.buff).await {
            error!("failed to read cdx line: {}", x);
            return None
        }

        if self.buff.is_empty() {
            debug!("cdx file ended");
            return None
        }

        let ret = CDXRecord::from_line(&self.buff);
        if let Ok(x) = ret{
            Some(x)
        } else {
            error!("failed to read cdx entry: {}", ret.err().unwrap());
            None
        }
    }
}