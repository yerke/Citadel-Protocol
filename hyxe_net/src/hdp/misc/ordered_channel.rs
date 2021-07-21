use crate::hdp::outbound_sender::UnboundedSender;
use hyxe_crypt::prelude::SecBuffer;
use std::collections::HashMap;
use crate::error::NetworkError;
use std::time::Instant;

pub struct OrderedChannel {
    sink: UnboundedSender<SecBuffer>,
    map: HashMap<u64, SecBuffer>,
    last_message_received: Option<u64>,
    #[allow(dead_code)]
    last_message_received_instant: Option<Instant>
}

impl OrderedChannel {
    pub fn new(sink: UnboundedSender<SecBuffer>) -> Self {
        Self { sink, map: HashMap::new(), last_message_received: None, last_message_received_instant: None }
    }

    #[allow(unused_results)]
    pub fn on_packet_received(&mut self, id: u64, packet: SecBuffer) -> Result<(), NetworkError> {
        let next_expected_message_id = self.last_message_received.clone().map(|r| r.wrapping_add(1)).unwrap_or(0);
        if next_expected_message_id == id {
            // we send this packet, then scan sequentially for any other packets that may have been delivered until hitting discontinuity
            self.send_then_scan(id, packet)?;

            /*
            if id == 0 {
                let ptr = self as *const OrderedChannel;
                tokio::task::spawn(async move {
                    use futures::StreamExt;
                    while let Some(_) = tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(std::time::Duration::from_millis(5000))).next().await {
                        let this = unsafe { &*ptr };
                        log::error!("Looking for: {:?}. Map has {} items", this.last_message_received.clone().map(|r| r.wrapping_add(1)), this.map.len());
                    }
                });
            }*/

            Ok(())
        } else {
            // we store. Since the next needed packet in order is not yet received, we store and return
            self.store_received_packet(id, packet);
            Ok(())
        }
    }

    #[allow(unused_results)]
    fn store_received_packet(&mut self, id: u64, packet: SecBuffer) {
        self.map.insert(id, packet);
        self.set_last_message_received_instant();
    }

    fn set_last_message_received_instant(&mut self) {
        self.last_message_received_instant = Some(Instant::now())
    }

    fn set_last_message_received(&mut self, id: u64) {
        self.last_message_received = Some(id)
    }

    fn send_then_scan(&mut self, new_id: u64, packet: SecBuffer) -> Result<(), NetworkError> {
        self.send_unconditional(new_id, packet)?;
        if self.map.len() != 0 {
            self.scan_send(new_id)
        } else {
            Ok(())
        }
    }

    // Assumes `last_arrived_id` has already been sent through the sink. This function will scan the elements in the hashmap sequentially, sending each enqueued packet, stopping once discontinuity occurs
    fn scan_send(&mut self, last_arrived_id: u64) -> Result<(), NetworkError> {
        let mut cur_scan_id = last_arrived_id.wrapping_add(1);
        let mut cnt = 0;
        loop {
            if let Some(next) = self.map.remove(&cur_scan_id) {
                self.send_unconditional(cur_scan_id, next)?;
                cur_scan_id = cur_scan_id.wrapping_add(1);
                cnt += 1;
            } else {
                break;
            }
        }

        log::info!("CNT: {}", cnt);

        Ok(())
    }

    fn send_unconditional(&mut self, new_id: u64, packet: SecBuffer) -> Result<(), NetworkError> {
        self.sink.unbounded_send(packet).map_err(|err| NetworkError::Generic(err.to_string()))?;
        self.set_last_message_received(new_id);
        self.set_last_message_received_instant();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::hdp::outbound_sender::unbounded;
    use crate::hdp::misc::ordered_channel::OrderedChannel;
    use rand::rngs::ThreadRng;
    use rand::prelude::SliceRandom;
    use hyxe_crypt::prelude::SecBuffer;
    use std::error::Error;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use futures::StreamExt;
    use std::time::Duration;
    use rand::Rng;

    fn setup_log() {
        std::env::set_var("RUST_LOG", "error,warn,info,trace");
        //std::env::set_var("RUST_LOG", "error");
        let _ = env_logger::try_init();
        log::trace!("TRACE enabled");
        log::info!("INFO enabled");
        log::warn!("WARN enabled");
        log::error!("ERROR enabled");
    }

    #[tokio::test]
    async fn smoke_ordered() -> Result<(), Box<dyn Error>> {
        setup_log();
        const COUNT: u8 = 100;
        let (tx, mut rx) = unbounded();
        let mut ordered_channel = OrderedChannel::new(tx.clone());
        let values_ordered = (0..COUNT).into_iter().map(|r| (r as _, SecBuffer::from(&[r] as &[u8]))).collect::<Vec<(u64, SecBuffer)>>();

        let recv_task = async move {
            let mut id = 0;
            while let Some(value) = rx.recv().await {
                log::info!("RECV: {:?}", value.as_ref());
                assert_eq!(id, value.as_ref()[0]);
                id += 1;

                if id >= COUNT {
                    return;
                }
            }
        };

        let recv_handle = tokio::task::spawn(recv_task);

        for (id, packet) in values_ordered {
            ordered_channel.on_packet_received(id, packet)?;
        }

        recv_handle.await?;


        Ok(())
    }

    #[tokio::test]
    async fn smoke_unordered() -> Result<(), Box<dyn Error>> {
        setup_log();
        const COUNT: usize = 1000;
        let (tx, mut rx) = unbounded();
        let mut ordered_channel = OrderedChannel::new(tx.clone());
        let mut values_ordered = (0..COUNT).into_iter().map(|r| (r as _, SecBuffer::from(&[(r % (u8::MAX as usize)) as u8] as &[u8]))).collect::<Vec<(u64, SecBuffer)>>();

        (&mut values_ordered[..]).shuffle(&mut ThreadRng::default());

        let values_unordered = values_ordered;

        //log::info!("Unordered input: {:?}", &values_unordered);
        let recv_task = async move {
            let mut id: usize = 0;
            while let Some(value) = rx.recv().await {
                log::info!("RECV: {:?}", value.as_ref());
                assert_eq!((id % u8::MAX as usize) as u8, value.as_ref()[0]);
                id += 1;

                if id >= COUNT {
                    return;
                }
            }
        };

        let recv_handle = tokio::task::spawn(recv_task);

        for (id, packet) in values_unordered {
            ordered_channel.on_packet_received(id, packet)?;
        }

        recv_handle.await?;

        Ok(())
    }

    #[tokio::test]
    async fn smoke_unordered_concurrent() -> Result<(), Box<dyn Error>> {
        //setup_log();
        const COUNT: usize = 1000000;
        let (tx, mut rx) = unbounded();
        let ordered_channel = OrderedChannel::new(tx.clone());
        let mut values_ordered = (0..COUNT).into_iter().map(|r| (r as _, SecBuffer::from(&[(r % (u8::MAX as usize)) as u8] as &[u8]))).collect::<Vec<(u64, SecBuffer)>>();

        (&mut values_ordered[..]).shuffle(&mut ThreadRng::default());

        let values_unordered = values_ordered;

        let ref ordered_channel = Arc::new(RwLock::new(ordered_channel));

        //log::info!("Unordered input: {:?}", &values_unordered);
        let recv_task = async move {
            let mut id: usize = 0;
            while let Some(value) = rx.recv().await {
                log::info!("RECV: {:?}", value.as_ref());
                assert_eq!((id % u8::MAX as usize) as u8, value.as_ref()[0]);
                id += 1;

                if id >= COUNT {
                    return;
                }
            }
        };

        let recv_handle = tokio::task::spawn(recv_task);

        tokio_stream::iter(values_unordered).for_each_concurrent(None, |(id, packet)| async move {
            let rnd = ThreadRng::default().gen_range(1, 10);
            tokio::time::sleep(Duration::from_millis(rnd)).await;
            ordered_channel.write().await.on_packet_received(id, packet).unwrap();
        }).await;

        recv_handle.await?;

        Ok(())
    }
}