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

use std::error::Error;

use clap::Parser;
use masstuffy::{database::DBManager, filesystem::{self, CollID}};

#[derive(Parser)]
struct Args {
    collection: String,

    #[arg(short, long)]
    dict_id: Option<u32>,
}

pub async fn main(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let args = Args::parse_from(&argv[1..]);

    let fs = filesystem::init().await?;
    let db = DBManager::new(&fs.get_database_conn_string());

    let coll = fs.get_collection(CollID::Slug(args.collection.clone())).await.unwrap();
    let coll = coll.read().await;

    let dict: Option<(String, u32)> =
    if let Some(dict_id) = args.dict_id {
        Some(("zstd".to_string(), dict_id))
    } else {
        None
    };

    coll.rebuild(dict, &db).await?;
    Ok(0)
}