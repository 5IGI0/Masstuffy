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

impl CDXRecord {
    pub fn from_warc(warc: &WarcRecord) -> anyhow::Result<Self> {
        Ok(CDXRecord{
            url: warc.get_target_uri(),
            record_type: warc.get_type()?,
            record_id: warc.get_record_id()?,
            date: warc.get_date()?.format("%Y%m%d").to_string(),
            file_name: None,
            file_offset: None
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