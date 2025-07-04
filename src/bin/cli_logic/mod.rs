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

use std::{env::args, error::Error};
use log::error;
use tokio;

mod init_fs;
mod push_records;
mod create_collection;
mod init_db;
mod get_record;
mod generate_dictionary;
mod search;
mod rebuild;
mod delete_collection;
mod create_token;
mod list_tokens;
mod delete_token;
mod grep;

fn print_help(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    print!(
r#"Usage: {} [SUB_COMMAND]

sub commands:
init_fs           - setup the current directory
create_collection - create a collection
push_records      - push new records to repository
init_db           - init database
get_record        - get record from its id
generate_dict     - generate dictionnary
search            - search records in db
rebuild           - rebuild a collection
delete_collection - delete a collection
create_token      - create an access token
list_tokens       - list access tokens
delete_token      - delete an access token
grep              - search text inside objects
"#,
    argv[0]);
    Ok(0)
}

#[tokio::main]
pub async fn main() {
    let argv: Vec<String> = args().collect();
    env_logger::init();

    if argv.len() < 2 {
        let _ = print_help(argv);
        return;
    }

    let ret = match argv[1].as_str() {
        "init_fs" => init_fs::main(argv),
        "create_collection" => create_collection::main(argv).await,
        "push_records" => push_records::main(argv).await,
        "get_record" => get_record::main(argv).await,
        "init_db" => init_db::main(argv).await,
        "generate_dict" => generate_dictionary::main(argv).await,
        "search" => search::main(argv).await,
        "rebuild" => rebuild::main(argv).await,
        "delete_collection" => delete_collection::main(argv).await,
        "create_token" => create_token::main(argv).await,
        "list_tokens" => list_tokens::main(argv).await,
        "delete_token" => delete_token::main(argv).await,
        "grep" => grep::main(argv).await,
        _ => print_help(argv),
    };

    if let Err(x) = ret {
        error!("{}", x);
    } else {
        std::process::exit(ret.ok().unwrap());
    }
}