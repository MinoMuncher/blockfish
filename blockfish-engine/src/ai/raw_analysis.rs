use std::sync::Arc;

use super::state::State;
use crate::{shape::ShapeTable, Config};

use super::b_star::Search;


pub fn analysis(shtb: Arc<ShapeTable>, cfg: Config, root: State) -> u16{
    let mut global_min = std::u16::MAX;

    let mut search = Search::new(&shtb, cfg.parameters);
    search.start(root);

    while search.node_count() < cfg.search_limit {

        match search.raw_step() {
            Ok(estimate)=>{
                if let Some(estimate) = estimate{
                    if estimate < global_min{
                        global_min = estimate
                    }
                }
            }

            Err(_) => break,
        }
    }
    return global_min
}