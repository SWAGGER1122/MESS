export type VideoResolutionPreset = "360p" | "720p" | "1080p";
export type VideoFpsPreset = 30 | 60;
export type VideoBitratePreset = 500 | 1500 | 3000 | 5000;

export interface MediaCallSettings {
  videoInputDeviceId: string;
  audioInputDeviceId: string;
  audioOutputDeviceId: string;
  resolution: VideoResolutionPreset;
  fps: VideoFpsPreset;
  maxBitrateKbps: VideoBitratePreset;
  noiseSuppression: boolean;
  echoCancellation: boolean;
  autoGainControl: boolean;
  mirrorLocalVideo: boolean;
}

export interface AvailableMediaDevices {
  cameras: MediaDeviceInfo[];
  microphones: MediaDeviceInfo[];
  speakers: MediaDeviceInfo[];
}

export const DEFAULT_CALL_SETTINGS: MediaCallSettings = {
  videoInputDeviceId: "default",
  audioInputDeviceId: "default",
  audioOutputDeviceId: "default",
  resolution: "720p",
  fps: 60,
  maxBitrateKbps: 3000,
  noiseSuppression: true,
  echoCancellation: true,
  autoGainControl: true,
  mirrorLocalVideo: true,
};

export function resolutionToDimensions(preset: VideoResolutionPreset): {
  width: number;
  height: number;
  label: string;
} {
  switch (preset) {
    case "360p":
      return { width: 640, height: 360, label: "360p (Экономия трафика)" };
    case "1080p":
      return { width: 1920, height: 1080, label: "1080p Full HD (Макс. четкость)" };
    case "720p":
    default:
      return { width: 1280, height: 720, label: "720p HD (Оптимально)" };
  }
}

export async function enumerateUserMediaDevices(): Promise<AvailableMediaDevices> {
  if (typeof navigator === "undefined" || !navigator.mediaDevices) {
    return { cameras: [], microphones: [], speakers: [] };
  }
  try {
    const all = await navigator.mediaDevices.enumerateDevices();
    return {
      cameras: all.filter((d) => d.kind === "videoinput"),
      microphones: all.filter((d) => d.kind === "audioinput"),
      speakers: all.filter((d) => d.kind === "audiooutput"),
    };
  } catch {
    return { cameras: [], microphones: [], speakers: [] };
  }
}

export async function acquireMediaStream(
  settings: MediaCallSettings,
  includeVideo: boolean
): Promise<MediaStream> {
  const dims = resolutionToDimensions(settings.resolution);

  const audioConstraints: MediaTrackConstraints = {
    deviceId:
      settings.audioInputDeviceId && settings.audioInputDeviceId !== "default"
        ? { ideal: settings.audioInputDeviceId }
        : undefined,
    noiseSuppression: settings.noiseSuppression,
    echoCancellation: settings.echoCancellation,
    autoGainControl: settings.autoGainControl,
  };

  const videoConstraints: MediaTrackConstraints | boolean = includeVideo
    ? {
        deviceId:
          settings.videoInputDeviceId && settings.videoInputDeviceId !== "default"
            ? { ideal: settings.videoInputDeviceId }
            : undefined,
        width: { ideal: dims.width },
        height: { ideal: dims.height },
        frameRate: { ideal: settings.fps },
      }
    : false;

  return navigator.mediaDevices.getUserMedia({
    audio: audioConstraints,
    video: videoConstraints,
  });
}

export async function acquireScreenShareStream(
  fps: VideoFpsPreset = 60
): Promise<MediaStream> {
  return navigator.mediaDevices.getDisplayMedia({
    video: {
      frameRate: { ideal: fps },
    },
    audio: true,
  });
}

/**
 * Generates a synthetic animated canvas video + silent audio stream as a graceful fallback
 * if the user's PC has no physical webcam attached or permission is denied, so calls and
 * video circles always work smoothly.
 */
export function createFallbackSyntheticStream(
  title: string,
  resolution: VideoResolutionPreset,
  fps: VideoFpsPreset
): MediaStream {
  const dims = resolutionToDimensions(resolution);
  const canvas = document.createElement("canvas");
  canvas.width = dims.width;
  canvas.height = dims.height;
  const ctx = canvas.getContext("2d");

  let frame = 0;
  const interval = Math.max(16, Math.floor(1000 / fps));

  const timer = window.setInterval(() => {
    if (!ctx) return;
    frame += 1;
    const grad = ctx.createLinearGradient(0, 0, canvas.width, canvas.height);
    grad.addColorStop(0, "#0f172a");
    grad.addColorStop(0.5, "#1e293b");
    grad.addColorStop(1, "#0f766e");
    ctx.fillStyle = grad;
    ctx.fillRect(0, 0, canvas.width, canvas.height);

    // Animated pulse ring
    const cx = canvas.width / 2;
    const cy = canvas.height / 2;
    const radius = 58 + Math.sin(frame * 0.08) * 14;

    ctx.beginPath();
    ctx.arc(cx, cy, radius + 20, 0, Math.PI * 2);
    ctx.strokeStyle = "rgba(56, 189, 248, 0.25)";
    ctx.lineWidth = 4;
    ctx.stroke();

    ctx.beginPath();
    ctx.arc(cx, cy, 56, 0, Math.PI * 2);
    ctx.fillStyle = "#0284c7";
    ctx.fill();

    ctx.fillStyle = "#ffffff";
    ctx.font = "bold 34px sans-serif";
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    ctx.fillText(title.slice(0, 2).toUpperCase(), cx, cy);

    ctx.font = "16px sans-serif";
    ctx.fillStyle = "rgba(226, 232, 240, 0.85)";
    ctx.fillText(`${resolution} • ${fps} FPS`, cx, cy + 95);
  }, interval);

  const stream = canvas.captureStream(fps);
  stream.getTracks().forEach((track) => {
    track.addEventListener("ended", () => window.clearInterval(timer));
  });
  return stream;
}
