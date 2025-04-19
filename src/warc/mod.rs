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

use std::{collections::HashMap, io::Write};
use tokio::{fs, io::{AsyncBufReadExt, AsyncRead, AsyncReadExt}};
use tokio::io::BufReader;

use anyhow::bail;
use chrono::Utc;
use log::warn;
use uuid::Uuid;

use crate::utils::open_compressed;

pub mod cdx;

#[derive(Debug)]
pub struct WarcRecord {
    headers: HashMap<String, Vec<String>>,
    body: Vec<u8>
}


fn must_overwrite_header(k: &str) -> bool {
    match k {
        "WARC-Type" => true,
        "WARC-Record-ID" => true,
        "WARC-Date" => true,
        _ => false
    }
}

impl WarcRecord {
    pub fn new(typ: String) -> Self {
        let mut warc = WarcRecord{
            headers: HashMap::new(),
            body: Vec::new()
        };

        warc.set_header("WARC-Type".to_string(), typ);
        warc.set_header("WARC-Record-ID".to_string(), format!("<urn:uuid:{}>", Uuid::new_v4()));
        warc.set_header("WARC-Date".to_string(), Utc::now().to_rfc3339());

        warc
    }

    // TODO: remove this function and make an iterator (so we don't copy)
    pub fn get_headers(&self) -> HashMap<String, Vec<String>> {
        self.headers.clone()
    }

    pub fn set_header(&mut self, k: String, v: String) {
        if k != "Content-Length" {
            self.headers.insert(k.to_string(),vec![v] );
        }
    }

    pub fn add_header(&mut self, k: &str, v: String) {
        if must_overwrite_header(k) {
            self.set_header(k.to_string(), v);
        } else if let Some(vv) = self.headers.get_mut(k) {
            vv.push(v);
        } else {
            self.set_header(k.to_string(), v);
        }
    }

    pub fn get_target_uri(&self) -> Option<String> {
        return self.get_header("WARC-Target-URI")
    }

    pub fn get_record_id(&self) -> anyhow::Result<String> {
        let tmp = self.get_header_or_err("WARC-Record-ID")?;
        Ok(tmp.trim_matches(|x| "<>".contains(x)).to_string())
    }

    pub fn get_content_len(&self) -> anyhow::Result<usize> {
        Ok(self.get_header_or_err("Content-Length")?.parse::<usize>()?)
    }

    pub fn get_date(&self) -> anyhow::Result<chrono::DateTime<Utc>> {
        Ok(self.get_header_or_err("WARC-Date")?.parse::<chrono::DateTime<chrono::Utc>>()?)
    }

    pub fn get_type(&self) -> anyhow::Result<String> {
        Ok(self.get_header_or_err("WARC-Type")?)
    }

    pub fn get_header_or_err(&self, k: &str) -> anyhow::Result<String> {
        if let Some(x) = self.get_header(k) {
            return Ok(x);
        } else {
            bail!("{} not found", k)
        }
    }

    pub fn get_header(&self, k: &str) -> Option<String> {
        if let Some(h) = self.headers.get(k) {
            h.get(0).cloned()
        } else {
            None
        }
    }

    pub fn set_body(&mut self, body: Vec<u8>) {
        self.body = body;
    }

    pub fn write_headers<W: Write>(&self, mut writer: W) -> std::io::Result<()> {
        for (k, v) in self.headers.iter() {
            for vv in v.iter() {
                writer.write_fmt(format_args!("{}: {}\r\n", k, vv))?;
            }
        };

        writer.write_fmt(format_args!("Content-Length: {}\r\n\r\n", self.body.len()))?;

        Ok(())
    }

    pub fn write_body<W: Write>(&self, mut writer: W) -> std::io::Result<()> {
        writer.write_all(&self.body)?;
        Ok(())
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut ret: Vec<u8> = "WARC/1.1\r\n".to_string().into_bytes().to_vec();

        self.write_headers(&mut ret).unwrap();
        self.write_body(&mut ret).unwrap();
        let _ = ret.write_all("\r\n\r\n".as_bytes());

        ret
    }
}

pub struct WarcReader {
    br: BufReader<Box<dyn AsyncRead + Unpin + Send>>
} 

impl WarcReader {
    pub fn from_fp(fp: fs::File) -> WarcReader {
        WarcReader{
            br: BufReader::new(Box::new(fp))
        }
    }

    pub fn from_bufreader(br: BufReader<Box<dyn AsyncRead + Unpin + Send>>) -> WarcReader{
        WarcReader{br}
    }

    pub async fn from_file(path: &str) -> anyhow::Result<WarcReader> {
        Ok(WarcReader{
            br: open_compressed(path).await?
        })
    }
    
    pub async fn async_next(&mut self) -> Option<WarcRecord> {
        let ret = read_record(&mut self.br).await;
        if let Ok(x) = ret {
            x
        } else {
            warn!("error while reading warc: {}", ret.unwrap_err());
            None
        }
    }
}

#[derive(PartialEq, Eq)]
enum ReadRecordState {
    WaitingWarcHeader,
    WaitingEndOfHeaders,
    _WaitingContent // this one is not used but still
}

pub async fn read_record<Bf: AsyncBufReadExt + Unpin>(mut br: Bf) -> anyhow::Result<Option<WarcRecord>>{
    let mut contentlen: Option<usize> = None;
    let mut ret = WarcRecord::new("".to_string());
    let mut state = ReadRecordState::WaitingWarcHeader;

    loop {
        let mut line_buffer = String::new();
        let num = br.read_line(&mut line_buffer).await?;

        if num == 0 {
            if state == ReadRecordState::WaitingWarcHeader {
                return Ok(None);
            } else {
                bail!("unexpected end of file");
            }
        }

        if state == ReadRecordState::WaitingWarcHeader {
            if line_buffer != "WARC/1.1\r\n" {
                bail!("expected 'WARC/1.1' but found '{}'", line_buffer.trim());
            } else {
                state = ReadRecordState::WaitingEndOfHeaders;
                continue;
            }
        }

        if line_buffer == "\r\n" {
            break;
        }

        if state != ReadRecordState::WaitingWarcHeader {
            if let Some(pos) = line_buffer.find(": ") {
                let key = &line_buffer[..pos];
                let value = &line_buffer[pos+2..line_buffer.len()-2];

                if key == "Content-Length" {
                    contentlen = Some(value.parse::<usize>()?);
                } else if key == "WARC-Type" {
                    ret.set_header(key.to_string(), value[..].to_string());  // cannot have 2 type
                } else {
                    ret.add_header(key, value[..].to_string());
                }
            } else {
                bail!("invalid header: {}", line_buffer.trim());
            }
        }
    }

    if let Some(len) = contentlen {
        let mut content: Vec<u8> = Vec::new();
        content.resize(len, 0);
        br.read_exact(&mut content[..]).await?;
        ret.set_body(content);
        let mut newlines: [u8; 4] = [0,0,0,0];
        br.read_exact(&mut newlines).await?;
        if newlines != "\r\n\r\n".as_bytes() {
            bail!("invalid body footer in warc record");
        }
    }

    Ok(Some(ret))
}