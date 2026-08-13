# FCast protocol v4 end to end (#248): the receiver announcing v4 (TXT v=4 + fp,
# Version{4} answers), driven by the real transmitters — the SDK terminal sender
# pinning our fingerprint through its own mDNS discovery and running a genuine
# TLS 1.3 session, the 2024 pre-SDK client still casting implicit-v1 beside it —
# and then FUTO's own conformance driver sweeping the green manifest below.
#
# The manifest is every case the portable (null-pipeline) build passes, held
# explicitly so a regression in any one of them fails CI. What is NOT in it and
# why (measured on the bench, 2026-08-09): render-pipeline cases (progress
# cadence, EOS autoplay, fetch failures — the null pipeline reports no position,
# never finishes media, never fetches; the two `*_url_resource_not_found_v4`
# cases now have the right *answer* behind them since #341, and are still
# unreachable here for that second reason), FCompanion transfers (#249), embedded
# track selection and subtitles and images (capability gaps the introduction
# states honestly), and playback speed (#250, refused over faked).
{ pkgs, self }:

let
  httpPort = 8080;
  friendlyName = "castaway-vm";
  advertised = "${friendlyName}#fcast";
  senders = import ./fcast-senders.nix { inherit pkgs; };

  greenManifest = [
    "cast_companion_missing_image_resource_not_found_v4"
    "cast_companion_missing_video_resource_not_found_v4"
    "cast_gif_v2"
    "cast_gif_v3"
    "cast_pause_during_load_v2"
    "cast_pause_during_load_v4"
    "cast_pause_resume_v2"
    "cast_pause_resume_v3"
    "cast_pause_resume_v4"
    "cast_photos_v2"
    "cast_photos_v3"
    "cast_photos_with_headers_v2"
    "cast_photos_with_headers_v3"
    "cast_photo_v2"
    "cast_photo_v3"
    "cast_photo_with_headers_v2"
    "cast_photo_with_headers_v3"
    "cast_queue_insert_remove_v4"
    "cast_queue_remove_current_v4"
    "cast_queue_select_no_load_v4"
    "cast_queue_select_out_of_range_v4"
    "cast_queue_v4"
    "cast_queue_with_headers_v4"
    "cast_seek_v4"
    "cast_simple_playlist"
    "cast_simple_playlist_with_headers"
    "cast_video_v2"
    "cast_video_v3"
    "cast_video_v4"
    "cast_video_with_headers_v2"
    "cast_video_with_headers_v3"
    "cast_video_with_start_speed_volume_v3"
    "connect_version_2"
    "connect_version_3"
    "connect_version_4"
    "empty_progress_interval_malformed_v4"
    "external_sub_no_media_rejected_v4"
    "flatbuf_before_handshake_closes"
    "garbage_flatbuf_closes_v4"
    "heartbeat"
    "heartbeat_v4"
    "invalid_opcode_error_v4"
    "multi_sender_load_broadcast_v4"
    "multi_sender_load_custom_metadata_broadcast_image_v4"
    "multi_sender_load_custom_metadata_broadcast_video_v4"
    "multi_sender_load_metadata_broadcast_image_v4"
    "multi_sender_load_metadata_broadcast_video_v4"
    "multi_sender_queue_insert_broadcast_v4"
    "multi_sender_queue_remove_broadcast_v4"
    "multi_sender_queue_select_broadcast_v4"
    "multi_sender_stop_broadcast_v4"
    "multi_sender_volume_broadcast_v4"
    "none_opcode_error_v4"
    "ping_before_version_closes"
    "queue_full_v4"
    "queue_insert_front_v4"
    "queue_load_no_start_index_v4"
    "queue_select_prefetched_video_v4"
    "seek_v3"
    "seek_while_paused_sends_progress_v4"
    "subscribe_media_item_start_1"
    "subscribe_media_item_start_2"
    "truncated_flatbuf_closes_v4"
    "unsubscribe_event_v3"
    "unsupported_opcode_error_v4"
    "version_downgrade_v5_to_v4"
    "version_zero_closes"
    "volume_clamped_high_v4"
    "volume_clamped_low_v4"
    "wrong_direction_messages_v4"
  ];
in
pkgs.testers.runNixOSTest {
  name = "castaway-fcast-v4";

  nodes = {
    receiver = { config, ... }: {
      imports = [ self.nixosModules.castaway ];
      services.castaway = {
        enable = true;
        inherit httpPort;
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.castaway-portable;
        logLevel = "info,castaway=debug,proto_fcast=debug";
        settings = {
          friendly_name = friendlyName;
          uuid = "0f8c2e10-0000-4000-8000-0000000c0572";
          interface = config.networking.primaryIPAddress;
          enable.fcast = true;
          fcast.announce_v4 = true;
        };
      };
    };

    sender = { ... }: {
      networking.firewall.enable = false;
      services.avahi = {
        enable = true;
        openFirewall = false;
      };
      environment.systemPackages = [ senders.fcastSender senders.fcastOldClient senders.fastDriver ];
    };
  };

  testScript = { nodes, ... }: ''
    kiosk = "${nodes.receiver.networking.primaryIPAddress}"

    def journal(pattern):
        receiver.succeed(
            "journalctl -u castaway --no-pager | grep -q " + repr(pattern)
        )

    start_all()

    with subtest("the service comes up announcing v4"):
        receiver.wait_for_unit("castaway.service")
        receiver.wait_for_open_port(46899)
        sender.wait_for_unit("multi-user.target")
        sender.wait_until_succeeds(
            "avahi-browse -rpt _fcast._tcp | grep '${friendlyName}' | grep ';46899;' | grep 'v=4' | grep -q 'fp='",
            timeout=60,
        )

    with subtest("the real SDK sender pins our fingerprint and runs a TLS session"):
        # Discovery by name is what carries the fp; the SDK refuses v4 without it.
        scan = sender.succeed("fcast scan --timeout 10")
        assert "${advertised}" in scan, scan
        sender.succeed(
            'fcast -n "${advertised}" play --mime-type video/mp4 --url http://example.invalid/v4.mp4 -t 5'
        )
        journal("fcast: v4 TLS up")
        journal("session: play")
        journal("http://example.invalid/v4.mp4")

    with subtest("v4 transport verbs reach the pipeline"):
        sender.succeed('fcast -n "${advertised}" pause')
        journal("null pipeline: CONTROL txn=Pause")
        sender.succeed('fcast -n "${advertised}" resume')
        journal("null pipeline: CONTROL txn=Play")
        sender.succeed('fcast -n "${advertised}" seek -t 42')
        journal("null pipeline: CONTROL txn=Seek(42s)")
        sender.succeed('fcast -n "${advertised}" set-volume -v 0.5')
        journal("null pipeline: CONTROL txn=Volume")
        sender.succeed('fcast -n "${advertised}" stop')
        journal("null pipeline: CONTROL txn=Stop")

    with subtest("a listening v4 sender is told what is playing"):
        sender.succeed(
            'fcast -n "${advertised}" play --mime-type video/mp4 --url http://example.invalid/listen.mp4'
        )
        out = sender.succeed(
            'timeout 8 fcast -n "${advertised}" listen 2>&1 || true'
        )
        assert "Source changed" in out, out
        assert "Playback state changed" in out, out

    with subtest("the 2024 pre-SDK client still casts implicit v1 beside v4"):
        sender.succeed(
            f"fcast-2024 -h {kiosk} play --mime_type video/mp4 --url http://example.invalid/legacy.mp4"
        )
        journal("http://example.invalid/legacy.mp4")

    with subtest("the conformance manifest stays green"):
        # One case per invocation, exactly as benched: fast's runner re-expands
        # names as substrings, and its run-all aborts at the first failure.
        cases = ${builtins.toJSON greenManifest}
        failures = []
        for case in cases:
            out = sender.execute(
                f"cd /tmp && fast -H {kiosk} -s ${senders.sampleMedia} run {case} 2>&1"
            )[1]
            if f"test {case} ... " not in out or "FAILED" in out.split(f"test {case} ... ")[-1].split("\n")[0]:
                failures.append(case)
        assert not failures, f"conformance regressions: {failures}"
  '';
}
