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

use std::{error::Error};
use clap::Parser;
use masstuffy::{database, filesystem, permissions::{TokenInfo, TokenPermission}};

#[derive(Parser)]
struct Args {
    /// token's comment
    #[arg(short, long, default_value_t=String::new())]
    comment: String,

    /// create token with _any_ read permission.
    #[arg(long, default_value_t = false)]
    read_any: bool,
    /// create token with _prefix_ read permission.
    #[arg(long)]
    read_prefix: Option<String>,
    /// create token with _list_ read permission.
    #[arg(long)]
    read_list: Option<Vec<String>>,

    
    /// create token with _any_ write permission.
    #[arg(long, default_value_t = false)]
    write_any: bool,
    /// create token with _prefix_ write permission.
    #[arg(long)]
    write_prefix: Option<String>,
    /// create token with _list_ write permission.
    #[arg(long)]
    write_list: Option<Vec<String>>,


    /// create token with _any_ delete permission.
    #[arg(long, default_value_t = false)]
    delete_any: bool,
    /// create token with _prefix_ delete permission.
    #[arg(long)]
    delete_prefix: Option<String>,
    /// create token with _list_ delete permission.
    #[arg(long)]
    delete_list: Option<Vec<String>>
}

pub async fn main(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let args = Args::parse_from(&argv[1..]);

    let mut read_perms: TokenPermission = TokenPermission::None;
    let mut write_perms: TokenPermission = TokenPermission::None;
    let mut delete_perms: TokenPermission = TokenPermission::None;

    if args.read_any {
        read_perms = TokenPermission::Any;
    } else if let Some(l) = args.read_list {
        read_perms = TokenPermission::List(l);
    } else if let Some(p) = args.read_prefix {
        read_perms = TokenPermission::Prefix(p)
    }

    if args.write_any {
        write_perms = TokenPermission::Any;
    } else if let Some(l) = args.write_list {
        write_perms = TokenPermission::List(l);
    } else if let Some(p) = args.write_prefix {
        write_perms = TokenPermission::Prefix(p)
    }

    if args.delete_any {
        delete_perms = TokenPermission::Any;
    } else if let Some(l) = args.delete_list {
        delete_perms = TokenPermission::List(l);
    } else if let Some(p) = args.delete_prefix {
        delete_perms = TokenPermission::Prefix(p)
    }

    let token = TokenInfo{
        token: uuid::Uuid::new_v4().to_string(),
        comment: args.comment,
        read_perms, write_perms, delete_perms};

    let fs = filesystem::init().await?;
    let db = database::DBManager::new(&fs.get_database_conn_string());

    println!("{}", token.token);
    db.create_permissions(token).await?;

    Ok(0)
}