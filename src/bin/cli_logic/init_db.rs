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

use masstuffy::filesystem::init;

pub fn main(_argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let fs = init()
        .expect("unable to initialise fs");

    let collections = fs.get_collection_list();
    
    for col in &collections {
        for record in fs.get_collection_cdx_iter(col)?.into_iter() {
            println!("{}", record)
        }
    }

    Ok(0)
}