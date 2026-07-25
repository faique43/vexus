use std::collections::HashMap;

pub const LIMIT: usize = 10;

pub struct Cache {
    map: HashMap<String, String>,
}

impl Cache {
    pub fn get(&self, key: &str) -> Option<&String> {
        lookup(&self.map, key)
    }
}

pub enum Mode { Fast, Slow }

pub trait Backend {
    fn load(&self, key: &str) -> String;
}

fn lookup<'a>(map: &'a HashMap<String, String>, key: &str) -> Option<&'a String> {
    map.get(key)
}
