import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";

export interface PublicIdentity {
  peer_id: string;
  ed25519_public_hex: string;
  x25519_public_hex: string;
  short_fingerprint: string;
  display_name: string | null;
}

export interface PreKeyBundle {
  peer_id: string;
  identity_ed25519_hex: string;
  identity_x25519_hex: string;
  signed_prekey_x25519_hex: string;
  signed_prekey_signature_hex: string;
  one_time_prekey_x25519_hex: string | null;
  timestamp_utc: number;
}

export interface IdentityBundle {
  mnemonic_phrase: string | null;
  public_identity: PublicIdentity;
  prekey_bundle: PreKeyBundle;
}

export interface NetworkStats {
  local_peer_id: string;
  listening_addrs: string[];
  relay_url: string | null;
  connected_peers_count: number;
  nat_status: string;
  stealth_mode_active: boolean;
  avg_rtt_ms: number;
  total_messages_sent: number;
  total_messages_received: number;
}

export interface NodeAddrTicket {
  peer_id: string;
  relay_url: string | null;
  direct_addresses: string[];
}

export interface CrdtChatMessage {
  id: string;
  room_id: string;
  sender_peer_id: string;
  text: string;
  timestamp_ms: number;
  edited_at_ms: number | null;
  deleted: boolean;
  reply_to_id: string | null;
  forwarded_from_peer: string | null;
  attachment_kind: "text" | "voice" | "video_circle" | "file" | "channel_post" | string;
  media_blake3_hash: string | null;
  media_mime: string | null;
  media_size_bytes: number | null;
  waveform: number[];
  reactions: Record<string, string[]>;
  channel_signature_hex: string | null;
}

export interface ChatRoomRecord {
  room_id: string;
  room_type: "direct_1on1" | "group_mls" | "channel_merkle";
  title: string;
  peer_id: string | null;
  folder: string;
  unread_count: number;
  pinned: boolean;
  muted: boolean;
  last_message_preview: string | null;
  last_activity_ms: number;
}

export interface SafetyNumberVerify {
  local_peer_id: string;
  remote_peer_id: string;
  numeric_blocks: string[];
  qr_payload_hex: string;
}

export type StealthTransportMode = "auto_adaptive" | "force_stealth443" | "local_mesh_only";

export interface StealthConfig {
  mode: StealthTransportMode;
  camouflage_sni_domain: string;
  blind_relay_endpoints: string[];
  enable_traffic_padding: boolean;
  chrome_user_agent: string;
}

export interface StealthTelemetry {
  active: boolean;
  mode: StealthTransportMode;
  active_sni_mask: string;
  active_blind_relay: string | null;
  obfuscated_frames_sent: number;
  obfuscated_frames_received: number;
  last_probe_latency_ms: number;
}

export interface IncomingPeerMessage {
  envelope_id: string;
  sender_peer_id: string;
  verified_signature: boolean;
  received_at_ms: number;
  chat_payload: {
    message_id: string;
    conversation_id: string;
    sender_peer_id: string;
    recipient_peer_id: string;
    timestamp_ms: number;
    text: string | null;
    reply_to_id: string | null;
    media_blake3_hash: string | null;
    media_mime: string | null;
    media_waveform: number[] | null;
    crdt_delta: number[] | null;
  };
}

// Detect whether we are running inside the native Tauri v2 WebView or browser dev preview
const isTauriRuntime = (): boolean =>
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export const tauriApi = {
  async unlockVault(masterPassword: string): Promise<IdentityBundle | null> {
    if (!isTauriRuntime()) return null;
    return invoke<IdentityBundle | null>("cmd_unlock_vault", {
      masterPassword,
    });
  },

  async generateIdentity(
    words: 12 | 24,
    passphrase?: string,
    displayName?: string
  ): Promise<IdentityBundle> {
    if (!isTauriRuntime()) {
      const demoWords12 =
        "cipherquantum nebula obsidian vortex paradox脉冲 aether zero trust stealth matrix";
      const demoWords24 =
        "abandon ability able about above absent absorb abstract absurd abuse access accident account accuse achieve acid acoustic acquire across act action actor actress actual";
      return {
        mnemonic_phrase: words === 24 ? demoWords24 : demoWords12,
        public_identity: {
          peer_id: "12D3KooWAetherZeroTrustNode9X8fK2mP7vL4qR1wZ5nJ3tB",
          ed25519_public_hex:
            "8f4e2b9a1c7d3e6f0a5b8c2d9e4f1a3b7c6d5e8f2a1b4c9d0e7f3a6b5c8d1e2f",
          x25519_public_hex:
            "4c9d0e7f3a6b5c8d1e2f8f4e2b9a1c7d3e6f0a5b8c2d9e4f1a3b7c6d5e8f2a1b",
          short_fingerprint: "8F4E2B9A1C7D3E6F",
          display_name: displayName || "overeign Node",
        },
        prekey_bundle: {
          peer_id: "12D3KooWAetherZeroTrustNode9X8fK2mP7vL4qR1wZ5nJ3tB",
          identity_ed25519_hex:
            "8f4e2b9a1c7d3e6f0a5b8c2d9e4f1a3b7c6d5e8f2a1b4c9d0e7f3a6b5c8d1e2f",
          identity_x25519_hex:
            "4c9d0e7f3a6b5c8d1e2f8f4e2b9a1c7d3e6f0a5b8c2d9e4f1a3b7c6d5e8f2a1b",
          signed_prekey_x25519_hex:
            "1a3b7c6d5e8f2a1b4c9d0e7f3a6b5c8d1e2f8f4e2b9a1c7d3e6f0a5b8c2d9e4f",
          signed_prekey_signature_hex: "f0e1d2c3b4a5968778695a4b3c2d1e0f",
          one_time_prekey_x25519_hex:
            "9e4f1a3b7c6d5e8f2a1b4c9d0e7f3a6b5c8d1e2f8f4e2b9a1c7d3e6f0a5b8c2d",
          timestamp_utc: Math.floor(Date.now() / 1000),
        },
      };
    }
    return invoke<IdentityBundle>("cmd_generate_identity", {
      words,
      passphrase: passphrase || null,
      displayName: displayName || null,
    });
  },

  async restoreIdentity(
    mnemonicPhrase: string,
    passphrase?: string,
    displayName?: string
  ): Promise<IdentityBundle> {
    if (!isTauriRuntime()) {
      return this.generateIdentity(12, passphrase, displayName);
    }
    return invoke<IdentityBundle>("cmd_restore_identity", {
      mnemonicPhrase,
      passphrase: passphrase || null,
      displayName: displayName || null,
    });
  },

  async spawnP2PNode(bindPort?: number): Promise<NetworkStats> {
    if (!isTauriRuntime()) {
      return {
        local_peer_id: "12D3KooWAetherZeroTrustNode9X8fK2mP7vL4qR1wZ5nJ3tB",
        listening_addrs: ["0.0.0.0:49152", "[::]:49152"],
        relay_url: "https://eu-central-1.relay.iroh.network.",
        connected_peers_count: 6,
        nat_status: "UDP Hole-Punched + Relay Ready",
        stealth_mode_active: false,
        avg_rtt_ms: 19,
        total_messages_sent: 14,
        total_messages_received: 28,
      };
    }
    return invoke<NetworkStats>("cmd_spawn_p2p_node", {
      bindPort: bindPort ?? null,
    });
  },

  async getNetworkStats(): Promise<NetworkStats> {
    if (!isTauriRuntime()) {
      return this.spawnP2PNode();
    }
    return invoke<NetworkStats>("cmd_get_network_stats");
  },

  async getLocalTicket(): Promise<NodeAddrTicket> {
    if (!isTauriRuntime()) {
      return {
        peer_id: "12D3KooWAetherZeroTrustNode9X8fK2mP7vL4qR1wZ5nJ3tB",
        relay_url: "https://eu-central-1.relay.iroh.network.",
        direct_addresses: ["192.168.1.42:49152"],
      };
    }
    return invoke<NodeAddrTicket>("cmd_get_local_ticket");
  },

  async registerPeerTicket(ticket: NodeAddrTicket, displayName?: string): Promise<void> {
    if (!isTauriRuntime()) return;
    return invoke<void>("cmd_register_peer_ticket", {
      ticket,
      displayName: displayName || null,
    });
  },

  async sendP2PMessage(params: {
    recipientPeerId: string;
    conversationId: string;
    text: string;
    replyToId?: string | null;
    attachmentKind?: string;
    mediaBytes?: number[];
    mediaMime?: string;
    waveform?: number[];
  }): Promise<CrdtChatMessage> {
    if (!isTauriRuntime()) {
      return {
        id: crypto.randomUUID(),
        room_id: params.conversationId,
        sender_peer_id: "LOCAL_SELF",
        text: params.text,
        timestamp_ms: Date.now(),
        edited_at_ms: null,
        deleted: false,
        reply_to_id: params.replyToId || null,
        forwarded_from_peer: null,
        attachment_kind: params.attachmentKind || "text",
        media_blake3_hash: params.mediaBytes
          ? "b3_9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
          : null,
        media_mime: params.mediaMime || null,
        media_size_bytes: params.mediaBytes ? params.mediaBytes.length : null,
        waveform: params.waveform || [],
        reactions: {},
        channel_signature_hex: null,
      };
    }
    return invoke<CrdtChatMessage>("cmd_send_p2p_message", {
      recipientPeerId: params.recipientPeerId,
      conversationId: params.conversationId,
      text: params.text,
      replyToId: params.replyToId ?? null,
      attachmentKind: params.attachmentKind ?? null,
      mediaBytes: params.mediaBytes ?? null,
      mediaMime: params.mediaMime ?? null,
      waveform: params.waveform ?? null,
    });
  },

  async listRoomMessages(roomId: string): Promise<CrdtChatMessage[]> {
    if (!isTauriRuntime()) return [];
    return invoke<CrdtChatMessage[]>("cmd_list_room_messages", { roomId });
  },

  async toggleReaction(params: {
    roomId: string;
    recipientPeerId?: string | null;
    messageId: string;
    emoji: string;
  }): Promise<CrdtChatMessage[]> {
    if (!isTauriRuntime()) return [];
    return invoke<CrdtChatMessage[]>("cmd_toggle_reaction", {
      roomId: params.roomId,
      recipientPeerId: params.recipientPeerId ?? null,
      messageId: params.messageId,
      emoji: params.emoji,
    });
  },

  async verifySafetyNumbers(remotePeerId: string): Promise<SafetyNumberVerify> {
    if (!isTauriRuntime()) {
      return {
        local_peer_id: "12D3KooWAetherZeroTrustNode9X8fK2mP7vL4qR1wZ5nJ3tB",
        remote_peer_id: remotePeerId,
        numeric_blocks: [
          "49201",
          "88314",
          "70295",
          "11482",
          "63950",
          "28471",
          "95038",
          "37164",
          "82649",
          "50317",
          "74920",
          "16835",
        ],
        qr_payload_hex:
          "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
      };
    }
    return invoke<SafetyNumberVerify>("cmd_verify_safety_numbers", { remotePeerId });
  },

  async configureStealth(config: StealthConfig): Promise<StealthTelemetry> {
    if (!isTauriRuntime()) {
      return {
        active: config.mode === "force_stealth443",
        mode: config.mode,
        active_sni_mask: config.camouflage_sni_domain,
        active_blind_relay: config.blind_relay_endpoints[0] || null,
        obfuscated_frames_sent: 42,
        obfuscated_frames_received: 57,
        last_probe_latency_ms: 29,
      };
    }
    return invoke<StealthTelemetry>("cmd_configure_stealth", { config });
  },

  async probeStealthSni(): Promise<number> {
    if (!isTauriRuntime()) return 27;
    return invoke<number>("cmd_probe_stealth_sni");
  },

  async onIncomingMessage(
    handler: (msg: IncomingPeerMessage) => void
  ): Promise<UnlistenFn> {
    if (!isTauriRuntime()) return () => {};
    return listen<IncomingPeerMessage>("p2p://incoming-message", (event) => {
      handler(event.payload);
    });
  },
};
