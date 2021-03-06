use crate::errors::OpenLimitsError;
use crate::exchange_ws::{CallbackHandle, ExchangeWs, OpenLimitsWs, Subscriptions};
use crate::model::websocket::{Subscription, WebSocketResponse};
use crate::shared::Result;
use futures::stream::BoxStream;
use std::sync::Arc;
use std::thread::sleep;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::Mutex;
use tokio::time::Duration;

pub type SubscriptionCallback<Response> =
    Arc<dyn Fn(&Result<WebSocketResponse<Response>>) + Sync + Send + 'static>;

pub type SubscriptionCallbackRegistry<E> = (
    Subscription,
    SubscriptionCallback<<E as ExchangeWs>::Response>,
);

pub struct ReconnectableWebsocket<E: ExchangeWs> {
    websocket: Arc<Mutex<OpenLimitsWs<E>>>,
    tx: UnboundedSender<()>,
    subscriptions: Arc<Mutex<Vec<SubscriptionCallbackRegistry<E>>>>,
}

impl<E: ExchangeWs + 'static> ReconnectableWebsocket<E> {
    pub async fn instantiate(params: E::InitParams, reattempt_interval: Duration) -> Result<Self> {
        let websocket = E::new(params.clone()).await?;
        let websocket = OpenLimitsWs { websocket };
        let websocket = Arc::new(Mutex::new(websocket));
        let subscriptions: Arc<Mutex<Vec<SubscriptionCallbackRegistry<E>>>> =
            Arc::new(Mutex::new(Default::default()));
        let (tx, mut rx) = unbounded_channel();
        {
            let websocket = Arc::downgrade(&websocket);
            let subscriptions = Arc::downgrade(&subscriptions);
            let tx = tx.clone();
            tokio::spawn(async move {
                while rx.recv().await.is_some() {
                    'reconnection: loop {
                        if let (Some(websocket), Some(subscriptions)) =
                            (websocket.upgrade(), subscriptions.upgrade())
                        {
                            if let Ok(new_websocket) = E::new(params.clone()).await {
                                let new_websocket = OpenLimitsWs {
                                    websocket: new_websocket,
                                };
                                let mut websocket = websocket.lock().await;
                                *websocket = new_websocket;

                                let subscriptions = { subscriptions.lock().await.clone() };
                                let subscriptions =
                                    subscriptions.iter().map(|(subscription, callback)| {
                                        let callback = callback.clone();
                                        let tx = tx.clone();
                                        websocket.subscribe(subscription.clone(), move |message| {
                                            if let Err(OpenLimitsError::SocketError()) =
                                                message.as_ref()
                                            {
                                                tx.send(()).ok();
                                            }
                                            callback(message)
                                        })
                                    });
                                if futures_util::future::join_all(subscriptions)
                                    .await
                                    .iter()
                                    .all(|subscription| subscription.is_ok())
                                {
                                    break 'reconnection;
                                }
                            }
                            sleep(reattempt_interval);
                        }
                    }
                }
            });
        }
        Ok(Self {
            websocket,
            tx,
            subscriptions,
        })
    }

    pub async fn create_stream_specific(
        &self,
        subscriptions: Subscriptions<E::Subscription>,
    ) -> Result<BoxStream<'static, Result<E::Response>>> {
        self.websocket
            .lock()
            .await
            .create_stream_specific(subscriptions)
            .await
    }

    pub async fn subscribe<
        F: Fn(&Result<WebSocketResponse<E::Response>>) + Sync + Send + Clone + 'static,
    >(
        &self,
        subscription: Subscription,
        callback: F,
    ) -> Result<CallbackHandle> {
        let tx = self.tx.clone();
        self.subscriptions
            .lock()
            .await
            .push((subscription.clone(), Arc::new(callback.clone())));
        self.websocket
            .lock()
            .await
            .subscribe(subscription, move |message| {
                if let Err(OpenLimitsError::SocketError()) = message.as_ref() {
                    tx.send(()).ok();
                }
                callback(message);
            })
            .await
    }

    pub async fn create_stream<S: Into<E::Subscription> + Clone + Send + Sync>(
        &self,
        subscriptions: &[S],
    ) -> Result<BoxStream<'static, Result<WebSocketResponse<E::Response>>>> {
        self.websocket
            .lock()
            .await
            .create_stream(subscriptions)
            .await
    }

    pub async fn disconnect(&self) {
        self.websocket.lock().await.disconnect().await;
    }
}
