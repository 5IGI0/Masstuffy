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

use std::io::Write;
use masstuffy::database::structs::DBWarcRecord;
use tide::{Request, Response};
use crate::server_logic::AppState;

/* FRONTEND HANDLERS */

pub async fn get_by_id(req: Request<AppState>) -> tide::Result {
    let db_rec = req.state().db.read().await
            .get_record_from_id(req.param("id").unwrap().to_string()).await?;
    
    unified_handler(req, db_rec).await
}

pub async fn get_by_url(req: Request<AppState>) -> tide::Result {
    let url = req.param("url").unwrap().to_string();
    let db_rec = req.state().db.read().await
            .get_record_from_uri(
                &req.param("date")?.to_string(),
                &url).await?;
    
    // TODO: redirect on different timestamp (might be disabled with flag)
    unified_handler(req, db_rec).await
}

/* COMMON LOGIC */

const RECORD_FLAGS_WARC_HEADER: u64 = 1<<0;
const RECORD_FLAGS_FORCE_DOWNLOAD: u64 = 1<<1;

async fn unified_handler(req: Request<AppState>, record: DBWarcRecord) -> tide::Result {
    let rec = req.state().fs.read().await.get_record(
        &record.collection, &record.filename, record.offset).await?.unwrap();

    let mut ret = Response::builder(200);
    let mut tmp_body: Vec<u8> = Vec::new();

    /* convert char flags to bit flags */
    let mut flags: u64 = 0;
    for c in req.param("flags").unwrap().chars().into_iter() {
        flags |= match c {
            'h' => RECORD_FLAGS_WARC_HEADER,
            'd' => RECORD_FLAGS_FORCE_DOWNLOAD,
            _ => 0
        }
    }

    if (flags&RECORD_FLAGS_WARC_HEADER) != 0 {
        rec.write_headers(&mut tmp_body).unwrap();
    }

    rec.write_body(&mut tmp_body).unwrap();

    if (flags&RECORD_FLAGS_WARC_HEADER) != 0 {
        // if we write the headers, lets write the newlines so it is a valid record.
        tmp_body.write_all("\r\n\r\n".as_bytes()).unwrap();
        if (flags&RECORD_FLAGS_FORCE_DOWNLOAD) == 0 {
        }
    }

    if (flags&RECORD_FLAGS_FORCE_DOWNLOAD) != 0 {
        ret = ret.header("Content-Type", "application/octet-stream");
    } else {
        if (flags&RECORD_FLAGS_WARC_HEADER) != 0 {
            ret = ret.header("Content-Type", "application/warc");
        } else {
            ret = ret.header("Content-Type",
                rec.get_header("Content-Type")
                .unwrap_or("application/octet-stream".to_string()));
        }
    }

    /* pass every record's headers to http response */
    let headers = rec.get_headers();
    for (k, v) in &headers {
        for vv in v {
            ret = ret.header(format!("Warc-Header-{}", k).as_str(), vv);
        }
    }

    Ok(ret.body(tmp_body).build())
}