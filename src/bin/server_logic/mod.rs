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

use masstuffy::{database::DBManager, filesystem::{self, FileSystem}, permissions::{assert_access, PermissionType}};
use serde::Serialize;
use tide::{utils::After, Body, Request, Response};
use tokio::sync::RwLock;

mod endpoints;


#[derive(Clone)]
struct AppState {
    fs: Arc<RwLock<FileSystem>>,
    db: Arc<RwLock<DBManager>>
}

#[derive(Serialize)]
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

    app.with(After(|mut res: Response| async {
        if let Some(err) = res.error() {
            res.set_body(format!("Error: {:?}", err));
        }
        Ok(res)
    }));

    app.at("/").get(server_status_handler);
    app.at("/id/:flags/:id").get(endpoints::record_getters::get_by_id);
    app.at("/url/:flags/:date/*url").get(endpoints::record_getters::get_by_url);
    app.at("/collections").get(endpoints::collections::list_collections);
    app.at("/search").get(endpoints::record_search::search_record);
    app.at("/collections").post(endpoints::collections::create_collection);
    app.at("/collection/:collection_uuid/records").post(endpoints::collections::push_records);
    app.at("/collection/:collection_uuid/raw_records").post(endpoints::collections::push_raw_records);
    app.at("/dictionary/:dict_id").get(endpoints::dictionaries::get_dictionary);
    app.listen(listen_addr).await.expect("server error");
}

async fn assert_access_http(req: &Request<AppState>, permtype: PermissionType, coll_slug: &String) -> anyhow::Result<()> {
    let token_header = req.header("Authorization");
    let token = // TODO: is there a proper way to do it?
    if let Some(h) = token_header {
        &h.as_str()[7..] // remove "Bearer "
    } else {
        ""
    };

    assert_access(
        &*req.state().db.read().await,
        &*req.state().fs.read().await,
        permtype, token,
        coll_slug).await
}