#[macro_use]
extern crate slog;
extern crate slog_term;
extern crate rusoto_core;
extern crate rusoto_ec2;
extern crate ssh2;
extern crate rusoto_credential;
extern crate tokio;
extern crate tempfile;
extern crate rayon;
extern crate failure;
#[macro_use(defer)] extern crate scopeguard;

use std::collections::HashMap;
use std::mem;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::Path;
use std::time;
use rand::distributions::Alphanumeric;
use rusoto_ec2::Ec2;
use failure::Error;
use failure::ResultExt;
use tokio::runtime::Handle;
use std::io::{Write};
use rayon::prelude::*;
use slog::{Drain, o, info};
pub struct SshConnection;

mod ssh;

/*
 * Machine struct is used to store information about the spot instances which are running in AWS.
 * private_ip: priva te ip address of the ec2 machine
 * public_dns: dns of the ec2 machine
 */
pub struct Machine {
    pub ssh: Option<ssh::Session>,
    pub instance_type: String,
    pub private_ip: String,
    pub public_dns: String,
    pub public_ip: String
}
 
/*
 * MachineSetup struct is used to stores description of the spot instances which will be launched in AWS.
 * it has following props: 
 * instance_type: possible type of ec2 machine available in aws
 * ami: possible machine images in aws
 * setup: A Box containing a trait object (Box<dyn Fn(&mut SshConnection) -> io::Result<()>>) that represents a function to set up the instance. This function takes a mutable reference to an SshConnection and returns an io::Result<()>.
 */
pub struct MachineSetup {
    instance_type: String,
    ami: String,
    setup: Box<dyn Fn(&mut ssh::Session) -> Result<(), Error> + Sync> 
}


 /* 
 * Follwing is the implementation of the new method for MachineSetup struct. 
 * It instantiates the MachineSetup with instance_type, ami and a setup method used to setup the machine when needed
 * The setup argument is a box containing a trait object which is a function to setup the instance.
 * THe trait bound for setup that is F, implies that the setup parameter must be a function or closure with a 'static lifetime
 * which means that the function/closure stored in the Box wil have lifetime of the program. 
 */
impl MachineSetup {
    pub fn new<F>(instance_type: &str, ami: &str, setup: F) -> Self
    where F: Fn(&mut ssh::Session) -> Result<(), Error> + 'static + Sync,
    {
        MachineSetup {
            instance_type: instance_type.to_string(),
            ami: ami.to_string(),
            setup: Box::new(setup)
        }
    }
}

/***
 * Struct Builder is used for instantiating the burst library with the list of machine sets descibed in the descriptors.
 * Each "machine set" is identified with a unique name, and machine set has n number of machines in it.
 * A machine in the "machine set" is configured with MachineSetup 
 * The max_duration denotes the time till which ec2 spot instances will run before being terminated.
 */
pub struct BurstBuilder {
    descriptors: HashMap<String, (MachineSetup, u32)>,
    log: slog::Logger,
    max_duration: i64,
}

/***
 * Default trait implementation of Burst Builder.
 * Helps in creating instances of BurstBuilder, instantiated with deafult values
 */
impl Default for BurstBuilder {
    fn default() -> Self {
        BurstBuilder {
            descriptors: Default::default(),
            log: slog::Logger::root(slog::Discard, o!()),
            max_duration: 60,
        }
    }
}

/*
 * Implementation block for Burst Builder.
 */
impl BurstBuilder {
    /*
     * The method "add_set" adds a new "machine set" to the burst builder struct by adding a entry to the 
     * descriptors field.
     */
    pub fn add_set(&mut self, name:&str, number: u32, description: MachineSetup) {
        // TODO : if name is already in use
        self.descriptors.insert(name.to_string(), (description, number));
    } 
    /*
     * The method "set_max_duration" modifies the max_duration attribute.
    */ 
    pub fn set_max_duration(&mut self, hours:u8) {
        self.max_duration = hours as i64 * 60;
    }

    pub fn set_logger(&mut self, log:slog::Logger) {
        self.log = log;
    }

    pub fn use_term_logger(&mut self) {
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::FullFormat::new(decorator).build();
        let drain = std::sync::Mutex::new(drain).fuse();

        self.log = slog::Logger::root(drain, o!());   
    }

    /*
     * The method "add_set" adds a new "machine set" to the burst builder struct by adding a entry to the 
     * descriptors field.
    */ 
    #[tokio::main]
    pub async fn run<F>(self, f: F) -> Result<(), Error>
    where F: FnOnce(HashMap<String, Vec<Machine>>) -> Result<(), Error> 
    {
        //let provider = rusoto::EnvironmentProvider;
        use rusoto_core::{Region};
        use rusoto_credential::{EnvironmentProvider};
       
        let log = &self.log;
        debug!(log, "connecting to ec2");
        /*
        * Here we create a Ec2Client object with a credentials provider and region etc  
        */
        let credentials_provider = EnvironmentProvider::default();
        let ec2 = rusoto_ec2::Ec2Client::new_with(
            rusoto_core::HttpClient::new()
            .context("falied to create tls session for the ec2 api client")?,
            credentials_provider,
            Region::UsEast1);

        info!(log, "spinning up tusnami");
        /**
         * Creating a security group
         */
        use rand::Rng;
        let mut group_name = String::from("burst_security_");
        group_name.extend(rand::thread_rng().sample_iter(&Alphanumeric).take(10).map(char::from));
        
        let mut req = rusoto_ec2::CreateSecurityGroupRequest::default();
        req.group_name = group_name.clone();
        req.description = "Temporary access groups for burst vms".to_string();
            
        trace!(log, "creating a security group name"; "name" => group_name);
        let res: rusoto_ec2::CreateSecurityGroupResult = ec2.create_security_group(req).await
                                                            .context("falied to create security groups for new machine")?;

        let group_id = res.group_id.expect("aws created security group with no group id");

        trace!(log, "created security group"; "id" => &group_id);

        // Adding rules to security group for ssh access and intra-machine communication
        let mut req = rusoto_ec2::AuthorizeSecurityGroupIngressRequest::default();
        req.group_id = Some(group_id.clone());
        req.ip_protocol = Some("tcp".to_string());
        req.from_port = Some(22);
        req.to_port = Some(22);
        req.cidr_ip = Some("0.0.0.0/0".to_string());
        trace!(log, "adding ssh access to security group");
        let _ = ec2.authorize_security_group_ingress(req).await
                    .context("falied to fill in security groups for new machine")?;

        // Cross-VM Talk
        let mut req = rusoto_ec2::AuthorizeSecurityGroupIngressRequest::default();
        req.group_id = Some(group_id.clone());
        req.ip_protocol = Some("tcp".to_string());
        req.from_port = Some(0);
        req.to_port = Some(65535);
        req.cidr_ip = Some("172.31.0.0/16".to_string());
        trace!(log, "adding internal VM access to security group");
        let _ = ec2.authorize_security_group_ingress(req).await
                    .context("falied to fill in security groups for new machine")?;

        trace!(log, "creating keypair");
        // creating a key pair 
        let mut req = rusoto_ec2::CreateKeyPairRequest::default();
        let mut key_name = String::from("burst_key_");
        key_name.extend(rand::thread_rng().sample_iter(&Alphanumeric).take(10).map(char::from));
        req.key_name = key_name.clone();

        let res = ec2.create_key_pair(req).await
                                .context("falied to generate new key pair")?;
        trace!(log, "created keypair"; "fingerprint" => res.key_fingerprint);
        let private_key = res.key_material.expect("aws did not generate key material for new key");

        // saving private key obtained to a temporary file for futhur usage like ssh
        let mut private_key_file = tempfile::NamedTempFile::new().context("failed to create temporary file for key-pair")?;
        private_key_file.write_all(private_key.as_bytes())
            .context("could not write private key to the file")?;

        if let Some(filename) = Path::new(private_key_file.path()).to_str() {
            trace!(log, "wrote keypair to file"; "filename" => filename) ;
        } else {

        }
       
        let mut setup_fns = HashMap::new();
        /*
        * Here we are calling requesting spot instances for all the machine sets and storing the request ids in spot_req_ids.
        */
        let mut id_to_name = HashMap::new();
        let mut spot_req_ids = Vec::new();
        debug!(log, "issuing spot requests");
        for (name, (setup, number)) in self.descriptors {
            let mut launch = rusoto_ec2::RequestSpotLaunchSpecification::default();
            launch.image_id = Some(setup.ami);
            launch.instance_type =Some(setup.instance_type);
            setup_fns.insert(name.clone(), setup.setup);

            launch.security_group_ids = Some(vec![group_id.clone()]);
            launch.key_name = Some(key_name.clone());
            

            let mut req = rusoto_ec2::RequestSpotInstancesRequest::default();          
            req.instance_count = Some(i64::from(number));
            // TODO
            // req.block_duration_minutes = Some(self.max_duration);
            req.launch_specification = Some(launch);
            let res = ec2.request_spot_instances(req).await
                                                     .context(format!("falied to request spot instance for {}", name))?;

            trace!(log, "issuing spot request for {}", name;"#" => number);
            if let Some(spot_instance_requests) = res.spot_instance_requests {
                // Handle spot_requests.
                spot_req_ids.extend(
                    spot_instance_requests.into_iter()
                    .filter_map(
                        |sir| sir.spot_instance_request_id
                    )
                    .map(|sir| {
                        trace!(log, "activated spot request"; "id" => &sir);
                        id_to_name.insert(sir.clone(), name.clone());
                        sir
                    })
                );
            } else {
                // Handle the case when spot_instance_requests is None.
            }
        }

        /*
         * Following code iterates over all the ec2 requests and checks whether if any one of the request is in open state.
         * If anyone of them is in "open state", it loops over again and again
         * If none of them is in "open state", then it collects the instance ids and breaks from the loop
         */
        let mut req = rusoto_ec2::DescribeSpotInstanceRequestsRequest::default();
        req.spot_instance_request_ids = Some(spot_req_ids);
        let instances: Vec<_>;
        let mut all_active;
        debug!(log, "waiting for instances to spwan");
        loop {
            trace!(log, "checking spot request status");
            let res = ec2.describe_spot_instance_requests(req.clone()).await;
            if let Err(e) = res {
                let msg = format!("{}",e);
                if msg.contains("The spot instance request ID") && msg.contains("does not exist") {
                    trace!(log, "spot instance request not yet ready");
                    continue;
                }
                else {
                    return Err(e).context("falied to describe spot instances")?;
                }
            }
            let res = res.expect("Error check above");
            if let Some(spot_instance_requests) = res.spot_instance_requests {
                // Handle spot_requests.
                let any_pending = spot_instance_requests
                                        .iter()
                                        .map(|sir| (sir, sir.state.as_ref().expect("spot request does not have state specified")))
                                        .any(|(sir, state)| {
                                            if state == "open" ||  (state == "active" && sir.instance_id.is_none()) {
                                                true
                                            }
                                            else {
                                                trace!(log, "spot instance request not yet ready"; "state" => state, "id" => &sir.spot_instance_request_id);
                                                false
                                            }
                                        });
            
                if !any_pending {
                    all_active = true;
                    instances = spot_instance_requests
                                    .into_iter()
                                    .filter_map(|sir| {
                                        if sir.state.as_ref().unwrap() == "active" {
                                            let name = id_to_name
                                            .remove(&sir.spot_instance_request_id.expect("spot request must have spot request id"))
                                            .expect("every spot request id is made for some machine set");

                                            let instance_id = sir.instance_id.unwrap();
                                            trace!(log, "spot request satisfied"; "setup" => &name, "iid" => &instance_id);
                                            id_to_name.insert(instance_id.clone(), name);
                                            Some(instance_id)
                                        }
                                        else {
                                            all_active=false;
                                            None
                                        }
                                    })
                                    .collect();
                    break;
                }
                else {                        
                }
            } 
        }

        let mut term_instances  = instances.clone();
        let ec2_clone  = ec2.clone();
        let log_clone  = log.clone();
        defer!{{
            /***
             * Lastly ec2 remote instance termination request is executed  to stop all the instances started.
             */
            debug!(log_clone, "terminating instances");
            let executor = Handle::current();
            let mut termination_req = rusoto_ec2::TerminateInstancesRequest::default();
            termination_req.instance_ids = mem::replace(&mut term_instances, Vec::new());
            executor.spawn({
                async move{
                    while let Err(e) = ec2_clone.terminate_instances(termination_req.clone()).await {
                        let msg = format!("{}", e);
                        if msg.contains("Pooled stream disconnected") || msg.contains("broken pipe") {
                            trace!(log_clone, "retrying instance termination");
                            continue
                        }
                        else {
                            warn!(log_clone, "failed to terminate instances : {:?}", e);
                        }
                    }
        
                    //debug!(log, "cleaning up temporary resources");
                    //trace!(log, "cleaning up terminating security group");
                    // let mut req = rusoto_ec2::DeleteSecurityGroupRequest::default();
                    // req.group_id = Some(group_id);
                    
                    // ec2.delete_security_group(req).await.context("failed to clean secuity group")?;
                    
                    //trace!(log, "cleaning up terminating keypair");
                    // let mut req = rusoto_ec2::DeleteKeyPairRequest::default();
                    // req.key_name = Some(key_name);
                    // ec2.delete_key_pair(req).await.context("failed to clean key pair")?;
                }
            });
        }}


        /*
        * Here once all the ec2 spot instance requests are satified, the instances are now starting or runing.
        * The spot instance requests are cancelled, to ensure that if anyone of the instances stops, the spot instance requests are not called again.
        * All the requests happen once and all the instances are requested/started only once.
        */
        trace!(log, "terminating spot requests");
        let mut cancel = rusoto_ec2::CancelSpotInstanceRequestsRequest::default();
        cancel.spot_instance_request_ids = req.spot_instance_request_ids.expect("this is set above");
        ec2.cancel_spot_instance_requests(cancel).await
        .context("falied to cancel spot instance request").map_err(|e| {
            warn!(log, "failed to cancel sopt instance requests: {:?}", e);
            e
        })?;


        /****
         * Here all the ec2 instances which are requested are iterated and checked where 
         * if all the requested ec2 machines are ready or not
         * it all not ready, then status of all the instances are requested again and checked
         * if all ready, then Machine structs are are populated with the config of the ec2 machines and stored in machines vector. 
         */
        let mut machines = HashMap::new();
        let mut desc_req: rusoto_ec2::DescribeInstancesRequest = rusoto_ec2::DescribeInstancesRequest::default();
        let mut all_ready = false;
        while !all_ready {
            machines.clear();
            all_ready = true;
            desc_req.instance_ids = Some(instances.clone());
            let res: rusoto_ec2::DescribeInstancesResult = ec2.describe_instances(desc_req.clone()).await
                                                                    .map_err(Error::from)
                                                                    .map_err(|e| e.context("falied to cancel spot instance request"))?;
            if let Some(res_reservations) = res.reservations {
                for reservations in res_reservations.into_iter() {
                    for instance in reservations.instances.unwrap_or_else(Vec::new) {
                        match instance {
                            rusoto_ec2::Instance {
                                instance_id: Some(instance_id),
                                instance_type: Some(instance_type),
                                private_ip_address: Some(private_ip),
                                public_dns_name: Some(public_dns),
                                public_ip_address: Some(public_ip),
                                ..
                            } => {
                                let machine = Machine{
                                    ssh:None,
                                    instance_type,
                                    private_ip,
                                    public_dns,
                                    public_ip
                                };
                                let name = id_to_name[&instance_id].clone();
                                trace!(log, "instance ready"; "set" => &name, "ip"=> &machine.public_ip);
                                machines.entry(name).or_insert_with(Vec::new).push(machine);
                            }
                            _=> { 
                                all_ready = false;
                            }
                        }
                    }
                }
                
            } else {
                // Handle the case when spot_instance_requests is None.
            }
            
        }
        // req.spot_instance_request_ids = 
        // ec 2.describe_spot_instance_requests(req)
        /***
         * Here for all the machines which are up and running,
         * one by one ssh connection is established to each of the remote ec2 machines  and certain commands are executed to verify if they are running properly.
         * First a tcp stream to the ssh server in the remote ec2 machine is established
         * then a ssh session is created using ssh2 crate, the tcp stream is associated with the sssh session, which will enabled the ssh session to connect to remote machine using tcp stream
         * finally a ssh handshake happens to initialize ssh session and it negotiates encryptin and other settings
         * finally authentication happens with ssh user agent authentication method 
         * Lastly a ssh channel is created for executing commands in the remote server and the output is also recieved & printed on local machine
         */
        let mut errors: Vec<Error> = Vec::new();
        if all_active
        {
            info!(log, "all machines instantiated; running setup routines");
            for (name,machines) in &mut machines {          
                let f = &setup_fns[name];  
                errors.par_extend(
                    machines
                                .par_iter_mut()
                                .map(|machine| -> Result<_, Error>{
                                    let mut sess = ssh::Session::connect(
                                        SocketAddr::new(
                                            machine.public_ip
                                            .parse::<IpAddr>()
                                            .context("machine ip is not an ip address")?, 
                                            22),
                                        // &format!("{}:22", machine.public_ip),
                                         private_key_file.path()
                                        )
                                    .context(format!(
                                        "falied to ssh to {} machine {}",
                                        name,
                                        machine.public_ip
                                        )).map_err(|e| {
                                            error!(log, "failed to ssh to {}:{}", name, machine.public_ip);
                                            e
                                        })?;

                                    debug!(log, "setting up {} instance", &name; "ip"=> &machine.public_ip);
                                    f(&mut sess)
                                    .context(format!(
                                        "setup routine for {} machine failed",
                                        name
                                    )).map_err(|e| {
                                        error!(log, "setup for {} machine failed", name);
                                        e
                                    })?; 
                                    info!(log, "finished setting up {} instance", &name; "ip"=> &machine.public_ip);
                                    Ok(())
                    }) 
                    .filter_map(Result::err)
                ); 
            }
            if errors.is_empty() {
                let start = time::Instant::now();
                info!(log, "quiet before storm");
                f(machines).context("main routi  ne failed").map_err(|e| {
                    crit!(log, "main tusnami failed");
                    e
                })?;
                info!(log, "power of the tsunami unleashed"; "duration" => start.elapsed().as_secs());
            }
               
        }

        debug!(log, "all done");
        errors.into_iter().next().map(|e| Err(e)).unwrap_or(Ok(()))         
    }
}

