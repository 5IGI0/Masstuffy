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

use std::{env::args, error::Error};
use tokio;

mod init_fs;
mod push_records;
mod create_collection;
mod init_db;

fn print_help(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    print!(
r#"Usage: {} [SUB_COMMAND]

sub commands:
init_fs           - setup the current directory
create_collection - create a collection
push_records      - push new records to repository
init_db           - init database
"#,
    argv[0]);
    Ok(0)
}

pub fn main() {
    let argv: Vec<String> = args().collect();
    env_logger::init();

    if argv.len() < 2 {
        let _ = print_help(argv);
        return;
    }

    let ret = match argv[1].as_str() {
        "init_fs" => init_fs::main(argv),
        "create_collection" => create_collection::main(argv),
        "push_records" => push_records::main(argv),
        "init_db" => {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(init_db::main(argv))
        },
        _ => print_help(argv),
    };

    if let Err(x) = ret {
        panic!("{}", x);
    } else {
        std::process::exit(ret.ok().unwrap());
    }
}