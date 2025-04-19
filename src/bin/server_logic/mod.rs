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
use std::sync::Arc;

use masstuffy::{database::DBManager, filesystem::{self, FileSystem}};
use serde::{Deserialize, Serialize};
use tide::{Body, Request, Response};
use tokio::sync::RwLock;

mod endpoints;


#[derive(Clone)]
struct AppState {
    fs: Arc<RwLock<FileSystem>>,
    db: Arc<RwLock<DBManager>>
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

#[tokio::main]
pub async fn main() {
    env_logger::init();

    let fs = filesystem::init().await.expect("unable to init filesystem");
    
    let listen_addr = fs.get_listen_addr();
    let database_conn = fs.get_database_conn_string();

    let state = AppState{
        fs: Arc::new(RwLock::new(fs)),
        db: Arc::new(RwLock::new(DBManager::new(&database_conn)))
    };
    
    let mut app = tide::with_state(state);
    app.at("/").get(server_status_handler);
    app.at("/id/:flags/:id").get(endpoints::record_getters::get_by_id);
    app.at("/url/:flags/:date/*url").get(endpoints::record_getters::get_by_url);
    app.at("/collections").get(endpoints::collections::list_collections);
    app.at("/collections").post(endpoints::collections::create_collection);
    app.listen(listen_addr).await.expect("server error");
}