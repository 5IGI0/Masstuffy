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
use clap::Parser;

use log::error;
use masstuffy::{filesystem::init, warc::WarcReader};

#[derive(Parser)]
struct Args {
    /// warc file holding records
    source: String,

    /// collection's slug to put records into
    destination: String
}

pub fn main(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let args = Args::parse_from(&argv[1..]);

    let mut fs = init()
        .expect("unable to initialise fs");

    if !fs.has_collection(&args.destination) {
        error!("collection `{}` doesn't exist", args.destination);
        return Ok(1);
    }

    for record in WarcReader::from_file(&args.source)? {
        let _ = fs.add_warc(&args.destination, &record);
    }

    Ok(0)
}