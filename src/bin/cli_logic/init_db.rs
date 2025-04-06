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

use std::error::Error;

use log::{error, info};
use masstuffy::{database::DBManager, filesystem::{init, CollID}};

pub async fn main(_argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let fs = init().await
        .expect("unable to initialise fs");

    let mut db = DBManager::new(&fs.get_database_conn_string());

    db.setup_db().await;

    let collections = fs.get_collection_list().await;
    
    for col in &collections {
        info!("inserting collection '{}'", col);
        // TODO: optimise
        if let Some(col) = fs.get_collection(CollID::Slug(col.clone())).await {
            let uuid = col.read().await.get_uuid();
            let mut reader = col.read().await.iter_cdx().await?;
            while let Some(record) = reader.async_next().await {
                if let Err(x) = db.insert_record(&uuid, &record).await {
                    error!("error when inserting record: {}", x);
                }
            }
        }
    }

    Ok(0)
}