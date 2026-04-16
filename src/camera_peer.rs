use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{Mutex, mpsc};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::media::Sample;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use gstreamer as gst;
use gstreamer::message::MessageView;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use image::RgbaImage;
use imageproc::drawing::draw_filled_circle_mut;

use crate::utils::SignalMessage;

pub async fn connect_camera_to_ws() {
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
        "rustwebrtc_video".to_string(),
    ));

    let audio_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: "audio/opus".to_string(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "".to_string(),
            rtcp_feedback: vec![],
            ..Default::default()
        },
        "audio".to_string(),
        "rustwebrtc_audio".to_string(),
    ));

    start_video_track(video_track.clone()).await.unwrap();
    start_audio_track(audio_track.clone()).await.unwrap();

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
        /// How many browser tabs may receive the shared camera tracks at once.
        const MAX_VIEWERS: usize = 2;

        let peer_connections: Arc<
            Mutex<HashMap<String, Arc<webrtc::peer_connection::RTCPeerConnection>>>,
        > = Arc::new(Mutex::new(HashMap::new()));
        let viewer_order: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));

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

                            // Shared tracks fan out to every PC still in the map. Allow up to
                            // MAX_VIEWERS; same `from` replaces its PC; a new `from` when full
                            // evicts the oldest viewer (front of `viewer_order`).
                            let mut to_close: Vec<Arc<webrtc::peer_connection::RTCPeerConnection>> =
                                Vec::new();
                            {
                                let mut map = peer_connections.lock().await;
                                let mut order = viewer_order.lock().await;
                                if let Some(old_pc) = map.remove(&from) {
                                    to_close.push(old_pc);
                                    order.retain(|id| id != &from);
                                }
                                while map.len() >= MAX_VIEWERS {
                                    let victim =
                                        order.pop_front().or_else(|| map.keys().next().cloned());
                                    let Some(victim) = victim else {
                                        break;
                                    };
                                    if let Some(vpc) = map.remove(&victim) {
                                        eprintln!(
                                            "[WebRTC] evicting viewer {victim} (max {MAX_VIEWERS})"
                                        );
                                        to_close.push(vpc);
                                    }
                                    order.retain(|id| id != &victim);
                                }
                            }
                            for old in to_close {
                                if let Err(e) = old.close().await {
                                    eprintln!("[WebRTC] close peer connection: {e:?}");
                                }
                            }

                            let pc = api.new_peer_connection(Default::default()).await.unwrap();
                            let pc = Arc::new(pc);

                            // Drop map entry when *this* Arc is closed/failed (same `from` may get a new
                            // PC later; do not remove the replacement using the old PC's callback).
                            let pc_for_state = pc.clone();
                            let pcs_map = peer_connections.clone();
                            let pcs_order = viewer_order.clone();
                            let from_state = from.clone();
                            pc.on_peer_connection_state_change(Box::new(
                                move |s: RTCPeerConnectionState| {
                                    let pc_for_state = pc_for_state.clone();
                                    let pcs_map = pcs_map.clone();
                                    let pcs_order = pcs_order.clone();
                                    let from_state = from_state.clone();
                                    Box::pin(async move {
                                        if matches!(
                                            s,
                                            RTCPeerConnectionState::Closed
                                                | RTCPeerConnectionState::Failed
                                        ) {
                                            let mut map = pcs_map.lock().await;
                                            if let Some(live) = map.get(&from_state) {
                                                if Arc::ptr_eq(live, &pc_for_state) {
                                                    map.remove(&from_state);
                                                    pcs_order
                                                        .lock()
                                                        .await
                                                        .retain(|id| id != &from_state);
                                                    println!(
                                                        "[WebRTC] removed {} from peer map ({s})",
                                                        from_state
                                                    );
                                                }
                                            }
                                        }
                                    })
                                },
                            ));

                            {
                                let mut map = peer_connections.lock().await;
                                let mut order = viewer_order.lock().await;
                                map.insert(from.clone(), pc.clone());
                                order.push_back(from.clone());
                            }

                            pc.add_track(video_track.clone()).await.unwrap();
                            pc.add_track(audio_track.clone()).await.unwrap();
                            println!("[WebRTC] Video track added to peer connection");

                            // the offers needs are recorded (sdp)
                            let remote_offer = webrtc::peer_connection::sdp::session_description::RTCSessionDescription::offer(sdp).unwrap();
                            pc.set_remote_description(remote_offer).await.unwrap();
                            println!("[WebRTC] Remote description set");

                            let write_clone = write.clone();
                            let from_clone = from.clone();
                            pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                                let write_clone = write_clone.clone();
                                let from_clone = from_clone.clone();
                                Box::pin(async move {
                                    if let Some(candidate) = c {
                                        println!("[ICE] Sending candidate to {}", from_clone);

                                        let candidate_json: Result<
                                            RTCIceCandidateInit,
                                            webrtc::Error,
                                        > = candidate.to_json();
                                        if let Ok(init) = candidate_json {
                                            let msg = SignalMessage::Candidate {
                                                candidate: init.candidate,
                                                sdp_mid: init.sdp_mid,
                                                sdp_mline_index: init.sdp_mline_index,
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
                                        }
                                    }
                                })
                            }));

                            // let transceivers = pc.get_transceivers().await;
                            // for transceiver in transceivers {
                            //     transceiver.set_direction(webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection::Sendonly).await;
                            // }

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

fn add_live_indicator(frame: &mut RgbaImage, frame_count: u32) {
    if frame_count % 60 != 0 {
        return;
    }
    let color = image::Rgba([255, 0, 0, 255]);
    let cx = (frame.width() as i32).saturating_sub(40).max(25);
    let cy = (frame.height() as i32 / 12).max(20);

    draw_filled_circle_mut(frame, (cx, cy), 25, color);
}

fn log_gst_bus_errors(bus: &gst::Bus, label: &str) {
    while let Some(msg) = bus.timed_pop_filtered(
        gst::ClockTime::ZERO,
        &[
            gst::MessageType::Error,
            gst::MessageType::Warning,
            gst::MessageType::Eos,
        ],
    ) {
        match msg.view() {
            MessageView::Error(err) => {
                eprintln!(
                    "[GStreamer] {label} ERROR from {:?}: {} — {:?}",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            MessageView::Warning(w) => {
                eprintln!(
                    "[GStreamer] {label} WARNING from {:?}: {}",
                    w.src().map(|s| s.path_string()),
                    w.error()
                );
            }
            MessageView::Eos(_) => {
                eprintln!("[GStreamer] {label} EOS");
            }
            _ => {}
        }
    }
}

pub async fn start_video_track(video_track: Arc<TrackLocalStaticSample>) -> anyhow::Result<()> {
    gst::init()?;

    /// Capture + encode resolution (4:3). Lower than 640×480 to ease Pi CPU / USB bandwidth.
    const VIDEO_WIDTH: i32 = 480;
    const VIDEO_HEIGHT: i32 = 360;

    #[cfg(feature = "mac")]
    let program_and_device = "avfvideosrc device-index=0";

    #[cfg(feature = "linux")]
    let program_and_device = "v4l2src device=/dev/video0";

    // Let the camera negotiate its native mode, then scale to RGBA@target size. Forcing
    // width/height/framerate directly off v4l2src often causes pull_sample failures on Pi UVC.
    let video_pipeline = gst::parse::launch(&format!(
        "{program_and_device} ! \
         videoconvert ! videoscale ! \
         video/x-raw,format=RGBA,width={VIDEO_WIDTH},height={VIDEO_HEIGHT} ! \
         appsink name=raw_sink emit-signals=true sync=false max-buffers=1 drop=true",
    ))?;

    let vpipeline = video_pipeline.dynamic_cast::<gst::Pipeline>().unwrap();
    let raw_sink = vpipeline
        .by_name("raw_sink")
        .unwrap()
        .dynamic_cast::<gst_app::AppSink>()
        .unwrap();

    // Create encoding pipeline
    let encode_pipeline = gst::parse::launch(&format!(
        "appsrc name=src is-live=true do-timestamp=true ! \
         queue max-size-buffers=4 max-size-time=0 leaky=downstream ! \
         videoconvert ! \
         video/x-raw,format=I420,width={VIDEO_WIDTH},height={VIDEO_HEIGHT} ! \
         x264enc speed-preset=ultrafast tune=zerolatency bitrate=800 key-int-max=30 ! \
         video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au ! \
         appsink name=sink emit-signals=true sync=false max-buffers=1 drop=true",
    ))?;

    let epipeline = encode_pipeline.dynamic_cast::<gst::Pipeline>().unwrap();
    let appsrc = epipeline
        .by_name("src")
        .unwrap()
        .dynamic_cast::<gst_app::AppSrc>()
        .unwrap();
    let encoded_sink = epipeline
        .by_name("sink")
        .unwrap()
        .dynamic_cast::<gst_app::AppSink>()
        .unwrap();

    // Configure appsrc
    appsrc.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", VIDEO_WIDTH)
            .field("height", VIDEO_HEIGHT)
            .field("framerate", gst::Fraction::new(30, 1))
            .build(),
    ));
    appsrc.set_property("is-live", true);

    vpipeline.set_state(gst::State::Playing)?;
    epipeline.set_state(gst::State::Playing)?;
    println!("[GStreamer] Pipelines started");

    // `pull_sample` blocks; keep it off the Tokio worker pool. A dedicated thread
    // runs capture + encode; encoded AU bytes go to async `write_sample` via channel.
    //
    // Use a *bounded* queue + `blocking_send` so if WebRTC falls behind we backpressure
    // GStreamer instead of growing an unbounded queue (that felt like "compound" slowdown
    // after refreshes). Only one capture thread exists for the process.
    const ENCODED_FRAME_QUEUE: usize = 2;
    let (tx, mut rx) = mpsc::channel::<Bytes>(ENCODED_FRAME_QUEUE);
    let video_track_async = video_track.clone();
    tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if video_track_async
                .write_sample(&Sample {
                    data,
                    duration: Duration::from_millis(33),
                    ..Default::default()
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    thread::spawn(move || {
        println!("[GStreamer] Video capture thread running");
        const W: u32 = VIDEO_WIDTH as u32;
        const H: u32 = VIDEO_HEIGHT as u32;
        const FRAME_BYTES: usize = (W * H * 4) as usize;
        let mut frame_count: u32 = 0;
        let mut scratch: Vec<u8> = vec![0u8; FRAME_BYTES];

        loop {
            let raw_ok = match raw_sink.pull_sample() {
                Ok(sample) => {
                    if let Some(buffer) = sample.buffer() {
                        if let Ok(map) = buffer.map_readable() {
                            let data = map.as_slice();
                            if data.len() == FRAME_BYTES {
                                scratch.copy_from_slice(data);
                                match RgbaImage::from_raw(W, H, scratch) {
                                    Some(mut frame) => {
                                        add_live_indicator(&mut frame, frame_count);
                                        if frame_count == 120 {
                                            frame_count = 0;
                                        }
                                        frame_count += 1;
                                        scratch = frame.into_raw();
                                        match gst::Buffer::with_size(scratch.len()) {
                                            Ok(mut gst_buf) => {
                                                {
                                                    let buffer_ref = gst_buf.get_mut().unwrap();
                                                    let mut mapw =
                                                        buffer_ref.map_writable().unwrap();
                                                    mapw.copy_from_slice(&scratch);
                                                }
                                                if let Err(flow) = appsrc.push_buffer(gst_buf) {
                                                    eprintln!(
                                                        "[GStreamer] appsrc push_buffer: {flow:?}"
                                                    );
                                                    if let Some(bus) = epipeline.bus() {
                                                        log_gst_bus_errors(&bus, "encode");
                                                    }
                                                    thread::sleep(Duration::from_millis(200));
                                                    continue;
                                                }
                                                true
                                            }
                                            Err(_) => false,
                                        }
                                    }
                                    None => {
                                        eprintln!(
                                            "[GStreamer] frame size mismatch (expected {FRAME_BYTES} bytes)"
                                        );
                                        scratch = vec![0u8; FRAME_BYTES];
                                        false
                                    }
                                }
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
                Err(e) => {
                    eprintln!("[GStreamer] raw appsink pull_sample: {e:?}");
                    if let Some(bus) = vpipeline.bus() {
                        log_gst_bus_errors(&bus, "capture");
                    }
                    thread::sleep(Duration::from_millis(200));
                    false
                }
            };

            if !raw_ok {
                continue;
            }

            match encoded_sink.pull_sample() {
                Ok(sample) => {
                    if let Some(buffer) = sample.buffer() {
                        if let Ok(map) = buffer.map_readable() {
                            let data = map.as_slice();
                            if tx.blocking_send(Bytes::copy_from_slice(data)).is_err() {
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[GStreamer] encoded appsink pull_sample: {e:?}");
                    if let Some(bus) = epipeline.bus() {
                        log_gst_bus_errors(&bus, "encode");
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }

        let _ = vpipeline.set_state(gst::State::Null);
        let _ = epipeline.set_state(gst::State::Null);
    });

    Ok(())
}

pub async fn start_audio_track(audio_track: Arc<TrackLocalStaticSample>) -> anyhow::Result<()> {
    #[cfg(feature = "mac")]
    let audio_prog = "osxaudiosrc";

    #[cfg(feature = "linux")]
    let audio_prog = "alsasrc device=hw:2,0";

    let audio_pipeline = gst::parse::launch(&format!(
        "{} ! audioconvert ! audioresample ! audio/x-raw,channels=1,rate=48000 ! queue ! opusenc ! appsink name=audio_sink",
        audio_prog
    ))?;

    let apipeline = audio_pipeline.dynamic_cast::<gst::Pipeline>().unwrap();
    let asink = apipeline
        .by_name("audio_sink")
        .unwrap()
        .dynamic_cast::<gst_app::AppSink>()
        .unwrap();

    let acaps = gst::Caps::builder("audio/x-opus")
        .field("rate", 48000i32)
        .field("channels", 1i32)
        .build();
    asink.set_caps(Some(&acaps));

    apipeline.set_state(gst::State::Playing)?;

    tokio::spawn(async move {
        println!("Started Audio Thread");
        loop {
            if let Ok(sample) = asink.pull_sample() {
                if let Some(buffer) = sample.buffer() {
                    if let Ok(map) = buffer.map_readable() {
                        let data = map.as_slice();
                        let _ = audio_track
                            .write_sample(&Sample {
                                data: data.to_vec().into(),
                                duration: Duration::from_millis(20),
                                ..Default::default()
                            })
                            .await;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });

    Ok(())
}
