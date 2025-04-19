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

use async_compression::tokio::bufread::{GzipDecoder, ZstdDecoder, XzDecoder};
use tokio::io::{AsyncRead, BufReader};
use anyhow::Result;

pub async fn open_compressed(path: &str) -> Result<BufReader<Box<dyn AsyncRead + Unpin + Send>>> {
    let fp = tokio::fs::File::open(path).await?;

    if path.ends_with(".gz") {
        let dec = GzipDecoder::new(BufReader::new(fp));
        return Ok(BufReader::new(Box::new(dec)));
    } else if path.ends_with(".zst") {
        let dec = ZstdDecoder::new(BufReader::new(fp));
        return Ok(BufReader::new(Box::new(dec)));
    } else if path.ends_with(".xz") {
        let dec = XzDecoder::new(BufReader::new(fp));
        return Ok(BufReader::new(Box::new(dec)));
    }

    Ok(BufReader::new(Box::new(fp)))
}