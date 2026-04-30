pub mod google {
    pub mod rpc {
        tonic::include_proto!("google.rpc");
    }
}

pub mod waitagent {
    pub mod remote {
        pub mod v1 {
            tonic::include_proto!("waitagent.remote.v1");
        }
    }
}

pub use waitagent::remote::v1;
