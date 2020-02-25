extern crate byteorder;
extern crate env_logger;
extern crate futures;
#[macro_use] extern crate log;
extern crate tokio;

pub mod packet;
pub mod packet_handler;
pub mod pipe;
pub mod server;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
