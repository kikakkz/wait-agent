pub mod google {
    pub mod rpc {
        tonic::include_proto!("google.rpc");
    }
}

pub mod waitagent {
    pub mod remote {
        pub mod v1 {
            // Generated messages mirror the whole protocol; a few request
            // types have no constructor yet (rust 1.91 dead_code reach).
            #![allow(dead_code)]
            tonic::include_proto!("waitagent.remote.v1");
        }
    }
}

pub use waitagent::remote::v1;
