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
use masstuffy::{constants::MASSTUFFY_DATE_FMT, database::DBManager, filesystem, warc::massaged_url::Match};

#[derive(Parser)]
struct Args {
    /// match _domain_ and its subdomains
    #[arg(long)]
    domain: Option<String>,
    /// match _host_ (same as domain but exact match)
    #[arg(long)]
    host: Option<String>,
    /// match uris with _port_
    #[arg(long)]
    port: Option<u16>,
    /// match any path starting with _path_
    #[arg(long)]
    path: Option<String>,
    /// match path exactly equal to _exact path_
    #[arg(long)]
    exact_path: Option<String>
}

pub async fn main(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let args = Args::parse_from(&argv[1..]);

    let fs = filesystem::init().await?;
    let db = DBManager::new(&fs.get_database_conn_string());

    let mut host = Match::None;
    let mut path = Match::None;

    if let Some(p) = args.exact_path {
        path = Match::ExactMatch(p)
    } else if let Some(p) = args.path {
        path = Match::PartialMatch(p)
    }

    if let Some(h) = args.host {
        host = Match::ExactMatch(h)
    } else if let Some(h) = args.domain {
        host = Match::PartialMatch(h)
    }

    let results = db.search(host, args.port, path, 100).await?;

    for r in &results {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            r.r#type,
            r.identifier,
            r.uri.as_deref().unwrap_or(""),
            r.date.format(MASSTUFFY_DATE_FMT).to_string(),
            r.collection
        );
    }

    Ok(0)
}