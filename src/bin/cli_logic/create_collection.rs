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

use log::error;
use masstuffy::filesystem::init;

#[derive(Parser)]
struct Args {
    /// warc file holding records
    collection: String,

    /// dictionary's id to use
    #[arg(short, long)]
    dict_id: Option<u32>,
}

pub async fn main(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let args = Args::parse_from(&argv[1..]);

    let mut fs = init().await
        .expect("unable to initialise fs");

    if fs.has_collection_slug(&args.collection).await {
        error!("collection {} already exists", args.collection);
        return Ok(1);
    }

    fs.create_collection(
        args.collection,
        if let Some(dict_id) = args.dict_id {
            Some(("zstd".to_string(), dict_id))
        } else {
            None
        }
    ).await?;

    Ok(0)
}