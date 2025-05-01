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

use masstuffy::{constants::MASSTUFFY_DATE_FMT, database::structs::DBWarcRecord, warc::massaged_url::Match};
use serde::Serialize;
use tide::{Request, Response};

use crate::server_logic::AppState;

// TODO: pagination
pub async fn search_record(req: Request<AppState>) -> tide::Result {
    let mut host = Match::None;
    let mut path = Match::None;
    let mut port: Option<u16> = None;

    for p in req.url().query_pairs() {
        match p.0.as_ref() {
            "host" => host = Match::PartialMatch(p.1.to_string()),
            "host_exact" => host = Match::ExactMatch(p.1.to_string()),
            "path" => path = Match::PartialMatch(p.1.to_string()),
            "path_exact" => host = Match::ExactMatch(p.1.to_string()),
            "port" => port = Some(p.1.parse::<u16>().unwrap_or(0)),
            _ => {}
        }
    }

    let records = req.state().db.read().await
        .search(host, port, path, 100).await?;

    
    format_response(records, req.param("format").as_deref().unwrap_or("json")).await
}

/* FORMATS */

async fn format_response(records: Vec<DBWarcRecord>, format: &str) -> tide::Result {
    match format {
        "json" => format_response_json(records).await,
        _ => format_response_json(records).await,
    }
}

#[derive(Serialize)]
struct RecordSearchJson{
    uri: Option<String>,
    identifier: String,
    r#type: String,
    collection: String,
    date: String

}

async fn format_response_json(records: Vec<DBWarcRecord>) -> tide::Result {
    let ret_records: Vec<RecordSearchJson> = records.iter().map(|d| {
        RecordSearchJson{
            identifier: d.identifier.clone(),
            uri: d.uri.clone(),
            r#type: d.r#type.clone(),
            collection: d.collection.clone(),
            date: d.date.format(MASSTUFFY_DATE_FMT).to_string()
        }
    }).collect();

    Ok(Response::builder(200)
        .body(serde_json::to_string(&ret_records)?)
        .content_type("application/json")
        .build())
}