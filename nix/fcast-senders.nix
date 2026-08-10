# The real FCast transmitters, Nix-pinned as test oracles (rule 9's memory note:
# banned at runtime, welcome on the bench). Shared by fcast-vm (the v1-v3 check)
# and fcast-v4-vm (#248).
{ pkgs }:

rec {
  # The reference repository, pinned at the commit the checked-in wire fixtures
  # were captured from (crates/proto-fcast/tests/fixtures/README.md).
  src = pkgs.fetchgit {
    url = "https://gitlab.futo.org/videostreaming/fcast.git";
    rev = "f22f72dcd62dbe7de401c6ddf1a0a3c2e1f11c37";
    hash = "sha256-MUCrrnd9jPtYZ98GEsCdGwg54HjR72IEESL5p/OV+PM=";
  };

  # The reference terminal sender — the fcast-sender-sdk stack Grayjay embeds.
  fcastSender = pkgs.rustPlatform.buildRustPackage {
    pname = "fcast-terminal-sender";
    version = "0-unstable-2026-08-03";
    inherit src;
    cargoHash = "sha256-xMyVUb6cYvbUFfjLYu/YRTGGWQLXb8MQXVulhUF+FLQ=";
    buildAndTestSubdir = "senders/terminal";
    doCheck = false;
    meta.description = "FCast reference terminal sender (test oracle only, never shipped)";
  };

  # FUTO's own conformance driver: 123 scripted protocol exchanges with expected
  # receiver answers, v1 through v4.
  fastDriver = pkgs.rustPlatform.buildRustPackage {
    pname = "fcast-fast";
    version = "0-unstable-2026-08-03";
    inherit src;
    cargoHash = "sha256-xMyVUb6cYvbUFfjLYu/YRTGGWQLXb8MQXVulhUF+FLQ=";
    buildAndTestSubdir = "tools/fast";
    doCheck = false;
    meta.description = "FCast conformance driver (test oracle only, never shipped)";
  };

  # The 2024 pre-SDK client from nixpkgs, renamed so both `fcast` binaries can
  # coexist on a sender node. Speaks implicit v1 (no Version frame at all).
  fcastOldClient = pkgs.runCommand "fcast-2024" { } ''
    mkdir -p $out/bin
    ln -s ${pkgs.fcast-client}/bin/fcast $out/bin/fcast-2024
  '';

  # Synthesized stand-ins for the fcast-sample-media repository: the conformance
  # cases fetch these over the driver's own file server, and the *protocol*
  # assertions don't care what the pixels are. Real content only matters to the
  # render-pipeline cases, which are excluded in the VM (see the manifest there).
  sampleMedia = pkgs.runCommand "fcast-sample-media" {
    nativeBuildInputs = [ pkgs.ffmpeg ];
  } ''
    mkdir -p $out/{audio,video,image,subs}
    ffmpeg -f lavfi -i testsrc=duration=10:size=320x240:rate=10 \
           -f lavfi -i sine=frequency=440:duration=10 \
           -c:v libx264 -preset ultrafast -c:a aac $out/video/BigBuckBunny.mp4
    for f in short_clip video_dual_video video_multi_track video_with_subs video_with_vobsub; do
      ffmpeg -i $out/video/BigBuckBunny.mp4 -c copy $out/video/$f.mkv
    done
    ffmpeg -f lavfi -i sine=frequency=440:duration=8 -c:a libmp3lame \
           $out/audio/Court_House_Blues_Take_1.mp3
    cp $out/audio/Court_House_Blues_Take_1.mp3 $out/audio/Dont_Go_Way_Nobody.mp3
    ffmpeg -f lavfi -i testsrc=duration=1:size=160x120:rate=1 -frames:v 1 $out/image/flowers.jpg
    cp $out/image/flowers.jpg $out/image/garden.jpg
    ffmpeg -f lavfi -i testsrc=duration=2:size=160x120:rate=5 $out/image/animated.gif
    printf '1\n00:00:01,000 --> 00:00:03,000\nHello\n' > $out/subs/sample_en.srt
    cp $out/subs/sample_en.srt $out/subs/sample_es.srt
    printf 'WEBVTT\n\n00:00.000 --> 00:03.000\nHello\n' > $out/subs/generated_dense.vtt
  '';
}
