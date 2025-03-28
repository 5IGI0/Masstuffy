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
use std::{fmt::format, sync::Arc};

use masstuffy::{constants::MASSTUFFY_DATE_FMT, database::DBManager, filesystem::{self, FileSystem}};
use serde::{Deserialize, Serialize};
use tide::{Body, Request, Response};
use tokio::sync::RwLock;

#[derive(Clone)]
struct AppState {
    fs: Arc<RwLock<FileSystem>>,
    db: Arc<RwLock<DBManager>>
}

async fn get_by_id(req: Request<AppState>) -> tide::Result {
    let cdx_rec = req.state().db.read().await
            .get_record_from_id(req.param("id").unwrap().to_string()).await?;
    
    let rec = req.state().fs.read().await.get_record(
        &cdx_rec.collection, &cdx_rec.filename, cdx_rec.offset)?.unwrap();

    Ok(Response::builder(200)
    .body(rec.serialize())
    .build())
}

async fn get_by_url(req: Request<AppState>) -> tide::Result {
    let url = req.param("url").unwrap().to_string(); // TODO: get GET parameters
    let cdx_rec = req.state().db.read().await
            .get_record_from_uri(
                &req.param("date")?.to_string(),
                &url).await?;
    
    let rec = req.state().fs.read().await.get_record(
        &cdx_rec.collection, &cdx_rec.filename, cdx_rec.offset)?.unwrap();

    let dt = rec.get_date()?.format(MASSTUFFY_DATE_FMT).to_string();
    if dt != req.param("date")? {
        return Ok(Response::builder(307).
            body("").
            header("Location", format!("/url/{}/{}/{}", dt, req.param("flags")?, url)).
            build())
    }

    Ok(Response::builder(200)
    .body(rec.serialize())
    .build())
}

#[derive(Deserialize, Serialize)]
struct ServerStatus {
    repository: String,
    version: String
}

async fn server_status_handler(_: Request<AppState>) -> tide::Result {
    Ok(Response::from(Body::from_json(&ServerStatus{
        repository: env!("CARGO_PKG_HOMEPAGE").to_string(),
        version: env!("CARGO_PKG_VERSION").to_string()
    }).expect("error serializing")))
}

pub fn main() {
    env_logger::init();

    let fs = filesystem::init().expect("unable to init filesystem"); //Arc<RwLock<FileSystem>> = ;
    
    let listen_addr = fs.get_listen_addr();
    let database_conn = fs.get_database_conn_string();

    let state = AppState{
        fs: Arc::new(RwLock::new(fs)),
        db: Arc::new(RwLock::new(DBManager::new(&database_conn)))
        };

    
    let mut app = tide::with_state(state);
    app.at("/").get(server_status_handler);
    app.at("/id/:flags/:id").get(get_by_id);
    app.at("/url/:date/:flags/*url").get(get_by_url);
    let rt = tokio::runtime::Runtime::new().expect("unable to start tokio runtime");
    rt.block_on(app.listen(listen_addr)).expect("failed block_on");
}