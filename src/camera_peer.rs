use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::media::Sample;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::utils::SignalMessage;

pub async fn connect_camera_to_ws() {
    let mut current_peer: Option<String> = None;
    let url = "ws://127.0.0.1:3000/ws";
    let (ws_stream, _) = connect_async(url).await.expect("Failed to connect");

    let video_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: "video/H264".to_string(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_string(),
            rtcp_feedback: vec![],
            ..Default::default()
        },
        "video".to_string(),
        "rustwebrtc".to_string(),
    ));

    start_video_track(video_track.clone()).await.unwrap();

    println!("[WS] Connected to signaling server");

    let (write, mut read) = ws_stream.split();
    let write = Arc::new(Mutex::new(write));

    let register = SignalMessage::Register {
        from: "camera_peer".to_string(),
    };
    let json = serde_json::to_string(&register).unwrap();

    if write.lock().await.send(json.clone().into()).await.is_err() {
        println!("[WS] Failed to register camera_peer");
    } else {
        println!("[WS] Registered camera_peer with server");
    }

    tokio::spawn(async move {
        let peer_connections: Arc<
            Mutex<HashMap<String, Arc<webrtc::peer_connection::RTCPeerConnection>>>,
        > = Arc::new(Mutex::new(HashMap::new()));

        println!("[WebRTC] Setting up peer connection");
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs().unwrap();

        // Explicitly register H264
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: "video/H264".to_string(),
                        clock_rate: 90000,
                        channels: 0,
                        sdp_fmtp_line:
                            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                                .to_string(),
                        rtcp_feedback: vec![],
                    },
                    payload_type: 109, // pick an unused payload type
                    ..Default::default()
                },
                RTPCodecType::Video,
            )
            .unwrap();

        let api = APIBuilder::new().with_media_engine(media_engine).build();

        // Handle incoming messages
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    // println!("[WS] Received message: {}", text);

                    if let Ok(from_id_json) = serde_json::from_str::<Value>(&text) {
                        if let Some(from_id) = from_id_json.get("from").and_then(|t| t.as_str()) {
                            println!("[WS] Current peer updated to: {}", from_id);
                            current_peer = Some(from_id.to_string());
                        }
                    }

                    let incoming: SignalMessage = match serde_json::from_str(&text) {
                        Ok(msg) => msg,
                        Err(e) => {
                            eprintln!("[WS] Failed to parse signaling message: {e}");
                            continue;
                        }
                    };

                    /* General flow of rtc connections
                     * Viewer sends offer → Contains their capabilities ("I can receive H.264 video")
                     * Camera calls set_remote_description(offer) → "I now know what the viewer wants"
                     * Camera creates answer → "Here's what I can provide back"
                     * Camera calls set_local_description(answer) → "This is my response"
                     */
                    match incoming.clone() {
                        SignalMessage::Offer { sdp, from, .. } => {
                            println!("[WebRTC] Received offer from {}", from);

                            let pc = api.new_peer_connection(Default::default()).await.unwrap();
                            let pc = Arc::new(pc);

                            // add newly connected peer to list
                            peer_connections
                                .lock()
                                .await
                                .insert(from.clone(), pc.clone());

                            pc.add_track(video_track.clone()).await.unwrap();
                            println!("[WebRTC] Video track added to peer connection");

                            // the offers needs are recorded (sdp)
                            let remote_offer = webrtc::peer_connection::sdp::session_description::RTCSessionDescription::offer(sdp).unwrap();
                            pc.set_remote_description(remote_offer).await.unwrap();
                            println!("[WebRTC] Remote description set");

                            let write_clone = write.clone();
                            let from_clone = from.clone();
                            let cp_clone = current_peer.clone();
                            pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                                let write_clone = write_clone.clone();
                                let from_clone = from_clone.clone();
                                let cp_clone = cp_clone.clone();
                                Box::pin({
                                    let value = cp_clone.clone();
                                    async move {
                                        if let Some(candidate) = c {
                                            if let Some(peer) = &value {
                                                println!("[ICE] Sending candidate to {}", peer);
                                                let msg = SignalMessage::Candidate {
                                                    candidate: candidate.to_string(),
                                                    from: "camera_peer".to_string(),
                                                    to: from_clone.to_string(),
                                                };
                                                let json = serde_json::to_string(&msg).unwrap();
                                                if let Err(e) = write_clone
                                                    .lock()
                                                    .await
                                                    .send(Message::Text(json.into()))
                                                    .await
                                                {
                                                    eprintln!(
                                                        "[ICE] Failed to send candidate: {:?}",
                                                        e
                                                    );
                                                }
                                            } else {
                                                println!("[ICE] No current peer for ICE candidate");
                                            }
                                        }
                                    }
                                })
                            }));

                            // Heres what THIS server can do.
                            let answer = pc.create_answer(None).await.unwrap();
                            pc.set_local_description(answer.clone()).await.unwrap();
                            println!("[WebRTC] Local description (answer) set");

                            let sdp_answer = SignalMessage::Answer {
                                sdp: answer.sdp,
                                from: "camera_peer".to_string(),
                                answer_to: from.to_string(),
                            };
                            let json = serde_json::to_string(&sdp_answer).unwrap();
                            if write.lock().await.send(json.into()).await.is_err() {
                                println!("[WS] Failed to send Answer message");
                            } else {
                                println!("[WS] Answer sent to {}", from);
                            }
                        }
                        SignalMessage::Candidate {
                            candidate, from, ..
                        } => {
                            println!("[ICE] Received ICE candidate from {}", from);

                            // Find the correct peer connection for this candidate
                            if let Some(pc) = peer_connections.lock().await.get(&from) {
                                let rtc_cand = RTCIceCandidateInit {
                                    candidate,
                                    sdp_mid: None,
                                    sdp_mline_index: None,
                                    username_fragment: None,
                                };
                                if let Err(e) = pc.add_ice_candidate(rtc_cand).await {
                                    eprintln!(
                                        "[ICE] Failed to add candidate for {}: {:?}",
                                        from, e
                                    );
                                } else {
                                    println!("[ICE] Candidate added successfully for {}", from);
                                }
                            } else {
                                println!("[ICE] No peer connection found for {}", from);
                            }
                        }

                        _ => {}
                    }
                }
                Ok(Message::Close(_)) => {
                    println!("[WS] Server closed connection");
                    break;
                }
                _ => {}
            }
        }
    });
}

pub async fn start_video_track(video_track: Arc<TrackLocalStaticSample>) -> anyhow::Result<()> {
    gst::init()?;

    #[cfg(feature = "mac")]
    let pipeline = gst::parse::launch(
        "avfvideosrc device-index=0 ! \
         videoconvert ! \
         video/x-raw,format=I420,width=640,height=480,framerate=30/1 ! \
         x264enc speed-preset=ultrafast tune=zerolatency bitrate=1000 key-int-max=30 ! \
         video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au ! \
         appsink name=sink emit-signals=true sync=false max-buffers=1 drop=true",
    )?;

    #[cfg(feature = "linux")]
    let pipeline = gst::parse::launch(
        "v4l2src device=/dev/video0 ! \
         videoconvert ! \
         video/x-raw,format=I420,width=640,height=480,framerate=30/1 ! \
         x264enc speed-preset=ultrafast tune=zerolatency bitrate=1000 key-int-max=30 ! \
         video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au ! \
         appsink name=sink emit-signals=true sync=false max-buffers=1 drop=true",
    )?;

    let pipeline = pipeline.dynamic_cast::<gst::Pipeline>().unwrap();
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .dynamic_cast::<gst_app::AppSink>()
        .unwrap();

    // Set caps on the appsink for consistency
    let caps = gst::Caps::builder("video/x-h264")
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build();
    sink.set_caps(Some(&caps));

    pipeline.set_state(gst::State::Playing)?;
    println!("[GStreamer] Pipeline started");

    tokio::spawn(async move {
        println!("Started Video Thread");
        loop {
            if let Ok(sample) = sink.pull_sample() {
                if let Some(buffer) = sample.buffer() {
                    if let Ok(map) = buffer.map_readable() {
                        let data = map.as_slice();

                        let _ = video_track
                            .write_sample(&Sample {
                                data: data.to_vec().into(),
                                duration: Duration::from_millis(33),
                                ..Default::default()
                            })
                            .await;
                    }
                }
            }
        }
    });

    Ok(())
}
