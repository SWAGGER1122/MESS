import React, { useEffect, useRef, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import {
  PhoneOff,
  Mic,
  MicOff,
  Video,
  VideoOff,
  MonitorUp,
  Settings,
  Volume2,
  Maximize2,
  Minimize2,
  Sliders,
  Check,
  Sparkles,
  Signal,
  RefreshCw,
} from "lucide-react";
import {
  MediaCallSettings,
  AvailableMediaDevices,
  VideoResolutionPreset,
  VideoFpsPreset,
  VideoBitratePreset,
  enumerateUserMediaDevices,
  acquireMediaStream,
  acquireScreenShareStream,
  createFallbackSyntheticStream,
  resolutionToDimensions,
} from "../lib/media-engine";

interface CallModalProps {
  isOpen: boolean;
  initialMode: "audio" | "video";
  contactTitle: string;
  contactUsername: string;
  avatarGradient: string;
  callSettings: MediaCallSettings;
  onUpdateSettings: (next: MediaCallSettings) => void;
  onEndCall: (durationSeconds: number, mode: "audio" | "video") => void;
}

export const CallModal: React.FC<CallModalProps> = ({
  isOpen,
  initialMode,
  contactTitle,
  contactUsername,
  avatarGradient,
  callSettings,
  onUpdateSettings,
  onEndCall,
}) => {
  const [isVideoEnabled, setIsVideoEnabled] = useState(initialMode === "video");
  const [isMicMuted, setIsMicMuted] = useState(false);
  const [isScreenSharing, setIsScreenSharing] = useState(false);
  const [showSettingsDrawer, setShowSettingsDrawer] = useState(false);
  const [isExpanded, setIsExpanded] = useState(false);
  const [callSeconds, setCallSeconds] = useState(0);
  const [devices, setDevices] = useState<AvailableMediaDevices>({
    cameras: [],
    microphones: [],
    speakers: [],
  });
  const [usingHardwareCamera, setUsingHardwareCamera] = useState(true);
  const [micLevel, setMicLevel] = useState(12);
  const [liveBitrateKbps, setLiveBitrateKbps] = useState<number>(
    callSettings.maxBitrateKbps
  );
  const [speakerVolume, setSpeakerVolume] = useState(90);

  const localVideoRef = useRef<HTMLVideoElement | null>(null);
  const remotePreviewRef = useRef<HTMLVideoElement | null>(null);
  const activeStreamRef = useRef<MediaStream | null>(null);
  const remoteStreamRef = useRef<MediaStream | null>(null);
  const audioContextRef = useRef<AudioContext | null>(null);
  const analyserTimerRef = useRef<number | null>(null);

  useEffect(() => {
    setIsVideoEnabled(initialMode === "video");
  }, [initialMode]);

  // Call duration timer & live bitrate meter
  useEffect(() => {
    if (!isOpen) {
      setCallSeconds(0);
      return;
    }
    const interval = window.setInterval(() => {
      setCallSeconds((s) => s + 1);
      // Simulate real-time encoder bitrate fluctuation around target preset
      const target = isVideoEnabled ? callSettings.maxBitrateKbps : 96;
      const jitter = Math.floor((Math.random() - 0.5) * (target * 0.08));
      setLiveBitrateKbps(Math.max(64, target + jitter));
    }, 1000);
    return () => window.clearInterval(interval);
  }, [isOpen, isVideoEnabled, callSettings.maxBitrateKbps]);

  const stopAllStreams = () => {
    if (analyserTimerRef.current) {
      window.clearInterval(analyserTimerRef.current);
      analyserTimerRef.current = null;
    }
    if (audioContextRef.current) {
      audioContextRef.current.close().catch(() => {});
      audioContextRef.current = null;
    }
    if (activeStreamRef.current) {
      activeStreamRef.current.getTracks().forEach((t) => t.stop());
      activeStreamRef.current = null;
    }
    if (remoteStreamRef.current) {
      remoteStreamRef.current.getTracks().forEach((t) => t.stop());
      remoteStreamRef.current = null;
    }
  };

  const attachAudioMeter = (stream: MediaStream) => {
    try {
      const audioTracks = stream.getAudioTracks();
      if (audioTracks.length === 0) return;

      const AudioCtx = window.AudioContext || (window as any).webkitAudioContext;
      if (!AudioCtx) return;

      const ctx = new AudioCtx();
      audioContextRef.current = ctx;
      const source = ctx.createMediaStreamSource(stream);
      const analyser = ctx.createAnalyser();
      analyser.fftSize = 64;
      source.connect(analyser);

      const data = new Uint8Array(analyser.frequencyBinCount);
      analyserTimerRef.current = window.setInterval(() => {
        analyser.getByteFrequencyData(data);
        const avg = data.reduce((acc, v) => acc + v, 0) / data.length;
        setMicLevel(Math.min(100, Math.round((avg / 140) * 100)));
      }, 90);
    } catch {
      // Ignore audio context restrictions
    }
  };

  const startOrUpdateMediaStream = async (
    wantVideo: boolean,
    settings: MediaCallSettings
  ) => {
    stopAllStreams();
    try {
      const stream = await acquireMediaStream(settings, wantVideo);
      activeStreamRef.current = stream;
      setUsingHardwareCamera(true);

      // Refresh device labels after permission granted
      const devList = await enumerateUserMediaDevices();
      setDevices(devList);

      stream.getAudioTracks().forEach((t) => {
        t.enabled = !isMicMuted;
      });
      attachAudioMeter(stream);

      if (localVideoRef.current) {
        localVideoRef.current.srcObject = stream;
      }
    } catch {
      // Fallback to synthetic stream if no physical camera/mic is connected
      setUsingHardwareCamera(false);
      const fallback = createFallbackSyntheticStream(
        "Вы",
        settings.resolution,
        settings.fps
      );
      activeStreamRef.current = fallback;
      if (localVideoRef.current) {
        localVideoRef.current.srcObject = fallback;
      }
    }

    // Remote contact canvas stream so video call layout looks alive
    const remoteSynthetic = createFallbackSyntheticStream(
      contactTitle,
      settings.resolution,
      settings.fps
    );
    remoteStreamRef.current = remoteSynthetic;
    if (remotePreviewRef.current) {
      remotePreviewRef.current.srcObject = remoteSynthetic;
    }
  };

  useEffect(() => {
    if (!isOpen) {
      stopAllStreams();
      return;
    }
    enumerateUserMediaDevices().then(setDevices);
    startOrUpdateMediaStream(
      initialMode === "video",
      callSettings
    );
    return () => {
      stopAllStreams();
    };
  }, [isOpen]);

  // Apply resolution/FPS/device changes on the fly
  const handleSettingsChange = async (partial: Partial<MediaCallSettings>) => {
    const next: MediaCallSettings = { ...callSettings, ...partial };
    onUpdateSettings(next);
    if (!isScreenSharing) {
      await startOrUpdateMediaStream(isVideoEnabled, next);
    }
  };

  const handleToggleMic = () => {
    const nextMuted = !isMicMuted;
    setIsMicMuted(nextMuted);
    if (activeStreamRef.current) {
      activeStreamRef.current.getAudioTracks().forEach((t) => {
        t.enabled = !nextMuted;
      });
    }
  };

  const handleToggleVideo = async () => {
    const nextVideo = !isVideoEnabled;
    setIsVideoEnabled(nextVideo);
    setIsScreenSharing(false);
    await startOrUpdateMediaStream(nextVideo, callSettings);
  };

  const handleToggleScreenShare = async () => {
    if (isScreenSharing) {
      setIsScreenSharing(false);
      await startOrUpdateMediaStream(isVideoEnabled, callSettings);
      return;
    }
    try {
      const screenStream = await acquireScreenShareStream(callSettings.fps);
      if (activeStreamRef.current) {
        activeStreamRef.current.getTracks().forEach((t) => t.stop());
      }
      activeStreamRef.current = screenStream;
      setIsScreenSharing(true);
      setIsVideoEnabled(true);
      if (localVideoRef.current) {
        localVideoRef.current.srcObject = screenStream;
      }
      screenStream.getVideoTracks()[0]?.addEventListener("ended", () => {
        setIsScreenSharing(false);
        startOrUpdateMediaStream(true, callSettings);
      });
    } catch {
      // User cancelled screen picker
    }
  };

  const formatTimer = (sec: number) => {
    const m = Math.floor(sec / 60)
      .toString()
      .padStart(2, "0");
    const s = (sec % 60).toString().padStart(2, "0");
    return `${m}:${s}`;
  };

  if (!isOpen) return null;

  const dims = resolutionToDimensions(callSettings.resolution);

  return (
    <AnimatePresence>
      <div className="fixed inset-0 bg-black/85 backdrop-blur-xl flex items-center justify-center p-4 z-50">
        <motion.div
          initial={{ scale: 0.94, opacity: 0 }}
          animate={{ scale: 1, opacity: 1 }}
          exit={{ scale: 0.94, opacity: 0 }}
          className={`relative bg-[#111924] border border-white/10 rounded-3xl shadow-2xl overflow-hidden flex flex-col transition-all ${
            isExpanded
              ? "w-full h-full max-w-[96vw] max-h-[94vh]"
              : "w-full max-w-4xl h-[640px]"
          }`}
        >
          {/* TOP CALL BAR */}
          <div className="px-6 py-4 bg-gradient-to-b from-black/70 to-transparent flex items-center justify-between z-20">
            <div className="flex items-center gap-3">
              <div
                className={`w-10 h-10 rounded-full bg-gradient-to-br ${avatarGradient} flex items-center justify-center font-bold text-sm text-white shadow`}
              >
                {contactTitle.slice(0, 2).toUpperCase()}
              </div>
              <div>
                <div className="flex items-center gap-2">
                  <h3 className="text-sm font-bold text-white">{contactTitle}</h3>
                  <span className="text-xs text-sky-400">{contactUsername}</span>
                </div>
                <div className="flex items-center gap-2 text-xs text-emerald-400">
                  <span className="w-2 h-2 rounded-full bg-emerald-400 animate-ping" />
                  <span>На связи • {formatTimer(callSeconds)}</span>
                  <span className="text-slate-400">•</span>
                  <span className="text-slate-300 font-medium">
                    {isScreenSharing
                      ? `Демонстрация экрана (${callSettings.fps} FPS)`
                      : isVideoEnabled
                      ? `HD Видео ${callSettings.resolution} • ${callSettings.fps} FPS`
                      : "Чистый голос HD"}
                  </span>
                </div>
              </div>
            </div>

            {/* Quality Pill & Window Controls */}
            <div className="flex items-center gap-2">
              <div
                onClick={() => setShowSettingsDrawer((v) => !v)}
                className="px-3 py-1.5 rounded-full bg-white/10 hover:bg-white/15 cursor-pointer flex items-center gap-2 text-xs text-slate-200 transition-colors"
                title="Настроить качество видео и звук"
              >
                <Signal className="w-3.5 h-3.5 text-emerald-400" />
                <span>
                  {dims.width}×{dims.height} • {(liveBitrateKbps / 1000).toFixed(1)} Мбит/с
                </span>
              </div>

              <button
                onClick={() => setShowSettingsDrawer((v) => !v)}
                className={`p-2.5 rounded-xl transition-colors ${
                  showSettingsDrawer
                    ? "bg-sky-500 text-white"
                    : "bg-white/10 hover:bg-white/15 text-slate-200"
                }`}
                title="Настройки видео, камеры и микрофона"
              >
                <Settings className="w-4 h-4" />
              </button>

              <button
                onClick={() => setIsExpanded((v) => !v)}
                className="p-2.5 rounded-xl bg-white/10 hover:bg-white/15 text-slate-200 transition-colors"
                title={isExpanded ? "Свернуть окно" : "На весь экран"}
              >
                {isExpanded ? (
                  <Minimize2 className="w-4 h-4" />
                ) : (
                  <Maximize2 className="w-4 h-4" />
                )}
              </button>
            </div>
          </div>

          {/* MAIN VIDEO / AUDIO STAGE */}
          <div className="flex-1 relative bg-[#0b1017] flex items-center justify-center overflow-hidden">
            {isVideoEnabled ? (
              <>
                {/* Main Primary Video (User's real webcam or screen share in full clarity) */}
                <video
                  ref={localVideoRef}
                  autoPlay
                  playsInline
                  muted
                  className={`w-full h-full object-cover ${
                    callSettings.mirrorLocalVideo && !isScreenSharing
                      ? "-scale-x-100"
                      : ""
                  }`}
                />

                {/* Picture-in-Picture Remote Contact Card */}
                <div className="absolute bottom-24 right-6 w-52 h-36 rounded-2xl overflow-hidden border-2 border-white/20 shadow-2xl bg-[#151f2c]">
                  <video
                    ref={remotePreviewRef}
                    autoPlay
                    playsInline
                    muted
                    className="w-full h-full object-cover"
                  />
                  <div className="absolute bottom-2 left-2.5 px-2 py-0.5 rounded-md bg-black/60 backdrop-blur-sm text-[11px] text-white font-medium">
                    {contactTitle}
                  </div>
                </div>

                {/* Badge if using virtual camera fallback */}
                <div className="absolute top-3 left-6 px-3 py-1 rounded-full bg-black/55 backdrop-blur-md border border-white/10 text-[11px] text-slate-200 flex items-center gap-1.5">
                  <Sparkles className="w-3.5 h-3.5 text-sky-400" />
                  <span>
                    {isScreenSharing
                      ? "Трансляция вашего экрана"
                      : usingHardwareCamera
                      ? `Ваша камера (${callSettings.resolution} @ ${callSettings.fps} FPS)`
                      : `Виртуальная камера (${callSettings.resolution} @ ${callSettings.fps} FPS)`}
                  </span>
                </div>
              </>
            ) : (
              /* Audio-only Mode with Animated Voice Rings */
              <div className="flex flex-col items-center justify-center text-center p-8">
                <div className="relative flex items-center justify-center">
                  <div
                    style={{
                      transform: `scale(${1 + (isMicMuted ? 0 : micLevel / 180)})`,
                    }}
                    className="w-40 h-40 rounded-full bg-sky-500/15 absolute transition-transform duration-100"
                  />
                  <div
                    className={`w-32 h-32 rounded-full bg-gradient-to-br ${avatarGradient} flex items-center justify-center text-4xl font-bold text-white shadow-2xl relative z-10`}
                  >
                    {contactTitle.slice(0, 2).toUpperCase()}
                  </div>
                </div>
                <h2 className="text-xl font-bold text-white mt-6">{contactTitle}</h2>
                <p className="text-sm text-emerald-400 mt-1">
                  Кристально чистый звук • Шумоподавление активно
                </p>
              </div>
            )}

            {/* SETTINGS DRAWER (VIDEO QUALITY, FPS, BITRATE, CAMERA/MIC SELECTOR) */}
            <AnimatePresence>
              {showSettingsDrawer && (
                <motion.div
                  initial={{ x: 360, opacity: 0 }}
                  animate={{ x: 0, opacity: 1 }}
                  exit={{ x: 360, opacity: 0 }}
                  className="absolute top-0 right-0 bottom-0 w-96 bg-[#151f2b]/95 backdrop-blur-2xl border-l border-white/10 p-5 overflow-y-auto z-30 space-y-5"
                >
                  <div className="flex items-center justify-between border-b border-white/10 pb-3">
                    <div className="flex items-center gap-2">
                      <Sliders className="w-4 h-4 text-sky-400" />
                      <h4 className="text-sm font-bold text-white">
                        Настройки видео и звука
                      </h4>
                    </div>
                    <button
                      onClick={() => setShowSettingsDrawer(false)}
                      className="text-xs text-sky-400 hover:underline font-medium"
                    >
                      Готово
                    </button>
                  </div>

                  {/* Resolution Preset */}
                  <div>
                    <label className="block text-xs font-semibold text-slate-300 mb-2">
                      Качество видео (Разрешение)
                    </label>
                    <div className="grid grid-cols-3 gap-2">
                      {(["360p", "720p", "1080p"] as VideoResolutionPreset[]).map(
                        (res) => (
                          <button
                            key={res}
                            onClick={() => handleSettingsChange({ resolution: res })}
                            className={`py-2 px-3 rounded-xl text-xs font-semibold border transition-all ${
                              callSettings.resolution === res
                                ? "bg-sky-500 text-white border-sky-400 shadow"
                                : "bg-[#0e1621] text-slate-300 border-white/10 hover:text-white"
                            }`}
                          >
                            {res === "1080p" ? "1080p FHD" : res === "720p" ? "720p HD" : "360p Эко"}
                          </button>
                        )
                      )}
                    </div>
                  </div>

                  {/* Frame Rate Preset */}
                  <div>
                    <label className="block text-xs font-semibold text-slate-300 mb-2">
                      Плавность картинки (Частота кадров)
                    </label>
                    <div className="grid grid-cols-2 gap-2">
                      {([30, 60] as VideoFpsPreset[]).map((fpsVal) => (
                        <button
                          key={fpsVal}
                          onClick={() => handleSettingsChange({ fps: fpsVal })}
                          className={`py-2 px-3 rounded-xl text-xs font-semibold border transition-all ${
                            callSettings.fps === fpsVal
                              ? "bg-sky-500 text-white border-sky-400 shadow"
                              : "bg-[#0e1621] text-slate-300 border-white/10 hover:text-white"
                          }`}
                        >
                          {fpsVal === 60 ? "60 FPS (Ультра-плавно)" : "30 FPS (Стандарт)"}
                        </button>
                      ))}
                    </div>
                  </div>

                  {/* Bitrate Selector */}
                  <div>
                    <label className="block text-xs font-semibold text-slate-300 mb-2">
                      Битрейт видеопотока
                    </label>
                    <div className="grid grid-cols-2 gap-2">
                      {(
                        [
                          { kbps: 500, label: "0.5 Мбит/с (Слабый интернет)" },
                          { kbps: 1500, label: "1.5 Мбит/с (Баланс)" },
                          { kbps: 3000, label: "3.0 Мбит/с (Высокая четкость)" },
                          { kbps: 5000, label: "5.0 Мбит/с (Максимум)" },
                        ] as { kbps: VideoBitratePreset; label: string }[]
                      ).map((item) => (
                        <button
                          key={item.kbps}
                          onClick={() =>
                            handleSettingsChange({ maxBitrateKbps: item.kbps })
                          }
                          className={`p-2 rounded-xl text-[11px] font-medium border text-left transition-all ${
                            callSettings.maxBitrateKbps === item.kbps
                              ? "bg-sky-500/20 text-white border-sky-400"
                              : "bg-[#0e1621] text-slate-400 border-white/10 hover:text-white"
                          }`}
                        >
                          {item.label}
                        </button>
                      ))}
                    </div>
                  </div>

                  {/* Camera Device Picker */}
                  <div>
                    <label className="block text-xs font-semibold text-slate-300 mb-1.5">
                      Камера
                    </label>
                    <select
                      value={callSettings.videoInputDeviceId}
                      onChange={(e) =>
                        handleSettingsChange({ videoInputDeviceId: e.target.value })
                      }
                      className="w-full px-3 py-2 rounded-xl bg-[#0e1621] border border-white/10 text-xs text-white focus:outline-none focus:border-sky-500"
                    >
                      <option value="default">Основная камера по умолчанию</option>
                      {devices.cameras.map((cam, i) => (
                        <option key={cam.deviceId || i} value={cam.deviceId}>
                          {cam.label || `Камера #${i + 1}`}
                        </option>
                      ))}
                    </select>
                  </div>

                  {/* Microphone Device Picker + Live Meter */}
                  <div>
                    <div className="flex items-center justify-between mb-1.5">
                      <label className="text-xs font-semibold text-slate-300">
                        Микрофон
                      </label>
                      <span className="text-[11px] text-emerald-400">
                        Уровень: {isMicMuted ? "Выкл" : `${micLevel}%`}
                      </span>
                    </div>
                    <select
                      value={callSettings.audioInputDeviceId}
                      onChange={(e) =>
                        handleSettingsChange({ audioInputDeviceId: e.target.value })
                      }
                      className="w-full px-3 py-2 rounded-xl bg-[#0e1621] border border-white/10 text-xs text-white focus:outline-none focus:border-sky-500"
                    >
                      <option value="default">Микрофон по умолчанию</option>
                      {devices.microphones.map((mic, i) => (
                        <option key={mic.deviceId || i} value={mic.deviceId}>
                          {mic.label || `Микрофон #${i + 1}`}
                        </option>
                      ))}
                    </select>
                    <div className="w-full h-1.5 bg-[#0e1621] rounded-full overflow-hidden mt-2">
                      <div
                        style={{ width: `${isMicMuted ? 0 : micLevel}%` }}
                        className="h-full bg-gradient-to-r from-emerald-400 to-sky-400 transition-all duration-75"
                      />
                    </div>
                  </div>

                  {/* Speaker Volume */}
                  <div>
                    <div className="flex items-center justify-between mb-1">
                      <label className="text-xs font-semibold text-slate-300 flex items-center gap-1.5">
                        <Volume2 className="w-3.5 h-3.5 text-sky-400" />
                        Громкость собеседника
                      </label>
                      <span className="text-xs text-slate-300">{speakerVolume}%</span>
                    </div>
                    <input
                      type="range"
                      min={0}
                      max={100}
                      value={speakerVolume}
                      onChange={(e) => setSpeakerVolume(Number(e.target.value))}
                      className="w-full accent-sky-500"
                    />
                  </div>

                  {/* Voice Enhancement Toggles */}
                  <div className="space-y-2 pt-1">
                    {(
                      [
                        {
                          key: "noiseSuppression",
                          title: "Умное шумоподавление",
                          desc: "Убирает гул вентилятора и клавиатуру",
                        },
                        {
                          key: "echoCancellation",
                          title: "Подавление эха",
                          desc: "Чистый звук даже без наушников",
                        },
                        {
                          key: "mirrorLocalVideo",
                          title: "Отзеркалить мою камеру",
                          desc: "Привычный вид селфи-камеры",
                        },
                      ] as const
                    ).map((opt) => (
                      <label
                        key={opt.key}
                        className="flex items-center justify-between p-2.5 rounded-xl bg-[#0e1621] border border-white/5 cursor-pointer"
                      >
                        <div>
                          <div className="text-xs font-medium text-white">
                            {opt.title}
                          </div>
                          <div className="text-[10px] text-slate-400">{opt.desc}</div>
                        </div>
                        <input
                          type="checkbox"
                          checked={callSettings[opt.key]}
                          onChange={(e) =>
                            handleSettingsChange({ [opt.key]: e.target.checked })
                          }
                          className="accent-sky-500 w-4 h-4"
                        />
                      </label>
                    ))}
                  </div>
                </motion.div>
              )}
            </AnimatePresence>
          </div>

          {/* BOTTOM CALL CONTROLS BAR */}
          <div className="px-6 py-4 bg-[#151f2b] border-t border-white/10 flex items-center justify-center gap-4 z-20">
            <button
              onClick={handleToggleMic}
              className={`w-13 h-13 p-3.5 rounded-2xl flex items-center justify-center transition-all ${
                isMicMuted
                  ? "bg-rose-500/20 text-rose-400 border border-rose-500/40"
                  : "bg-white/10 hover:bg-white/15 text-white"
              }`}
              title={isMicMuted ? "Включить микрофон" : "Выключить микрофон"}
            >
              {isMicMuted ? <MicOff className="w-5 h-5" /> : <Mic className="w-5 h-5" />}
            </button>

            <button
              onClick={handleToggleVideo}
              className={`w-13 h-13 p-3.5 rounded-2xl flex items-center justify-center transition-all ${
                !isVideoEnabled
                  ? "bg-white/10 text-slate-400 hover:text-white"
                  : "bg-sky-500 text-white shadow-lg shadow-sky-500/25"
              }`}
              title={isVideoEnabled ? "Выключить камеру" : "Включить камеру"}
            >
              {isVideoEnabled ? (
                <Video className="w-5 h-5" />
              ) : (
                <VideoOff className="w-5 h-5" />
              )}
            </button>

            <button
              onClick={handleToggleScreenShare}
              className={`w-13 h-13 p-3.5 rounded-2xl flex items-center justify-center transition-all ${
                isScreenSharing
                  ? "bg-emerald-500 text-white shadow-lg shadow-emerald-500/25"
                  : "bg-white/10 hover:bg-white/15 text-white"
              }`}
              title="Демонстрация экрана"
            >
              <MonitorUp className="w-5 h-5" />
            </button>

            <button
              onClick={() => setShowSettingsDrawer((v) => !v)}
              className={`px-4 py-3.5 rounded-2xl flex items-center gap-2 text-xs font-semibold transition-all ${
                showSettingsDrawer
                  ? "bg-sky-500 text-white"
                  : "bg-white/10 hover:bg-white/15 text-slate-200"
              }`}
            >
              <Settings className="w-4 h-4" />
              <span>Качество ({callSettings.resolution})</span>
            </button>

            <button
              onClick={() => onEndCall(callSeconds, isVideoEnabled ? "video" : "audio")}
              className="px-6 py-3.5 rounded-2xl bg-rose-600 hover:bg-rose-500 text-white font-bold text-xs flex items-center gap-2 shadow-lg shadow-rose-600/30 transition-all"
            >
              <PhoneOff className="w-4 h-4" />
              <span>Завершить</span>
            </button>
          </div>
        </motion.div>
      </div>
    </AnimatePresence>
  );
};
