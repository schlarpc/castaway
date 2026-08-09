# DIAL's positive discovery path (#202, item 4): a DIAL-enabled build — which since D55
# is simply `packages.default`, the full Electron kiosk — answers a sender's targeted
# M-SEARCH from another host, serves the description that M-SEARCH points at with its
# `Application-URL`, and does it under a root-device USN that does not collide with the
# DLNA renderer beside it on the same responder.
#
# A separate check from `integration-vm` on purpose. That one boots `castaway-portable`
# and asserts DIAL's *absence* — the D27 honesty property of a build with no browser —
# and both properties matter, so neither node can replace the other. This one carries the
# whole kiosk (Electron, wgpu, winit) into a headless VM, which takes three pieces of
# scaffolding the portable node has no use for:
#
#   - Xvfb. The kiosk is a winit window; winit needs a display server, and the #98 note
#     anticipated exactly this ("a `render` build with Xvfb").
#   - lavapipe, by the same three variables `checks.test` uses: the kiosk's compositor is
#     wgpu and the VM has no GPU, so the software Vulkan ICD is the adapter.
#   - Two of the module's hardening switches relaxed, each for a cause named at the
#     switch: the X socket lives in /tmp, and Chromium's namespace sandbox needs
#     namespaces.
#
# What this deliberately does NOT assert: pixels, page loads, or the Lounge. The browser
# under lavapipe cannot produce the zero-copy GPU frames the kiosk composites (D36/#64
# logs exactly that), and YouTube needs the real internet — that path stays with
# `nix run .#yt-selfplay`. What must hold here is the discovery contract a sender walks
# before any of that: M-SEARCH → 200 OK → LOCATION → Application-URL, on a real LAN.
{ pkgs, self }:

let
  httpPort = 8080;
  friendlyName = "castaway-vm";
  # The panel's configured UUID — DLNA's root device advertises it verbatim, and DIAL's
  # is derived from it (`device_uuid`, v5 over this namespace). The test script computes
  # the same derivation with Python's uuid5 so the expected USNs are written down
  # independently of the code under test.
  configUuid = "0f8c2e10-0000-4000-8000-0000000c0572";

  ssdpSearch = import ./ssdp-search.nix { inherit pkgs; };
in
pkgs.testers.runNixOSTest {
  name = "castaway-dial";

  nodes = {
    receiver = { config, lib, ... }: {
      imports = [ self.nixosModules.castaway ];

      # Electron plus the kiosk compositor; the portable node runs in the default 1G.
      virtualisation.memorySize = 4096;
      virtualisation.cores = 2;

      # The kiosk window needs a display server; nothing needs it to be visible.
      # `-ac` because the client is a DynamicUser with no Xauthority to present.
      systemd.services.xvfb = {
        description = "virtual display for the castaway kiosk";
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          ExecStart = "${pkgs.xorg.xvfb}/bin/Xvfb :99 -screen 0 1280x720x24 -ac";
          Restart = "on-failure";
        };
      };

      services.castaway = {
        enable = true;
        inherit httpPort;
        # No `package`: the default IS the DIAL-enabled build (D55) — the full kiosk
        # with the Electron browser DIAL needs as a launch target (D27). Pinning
        # something lighter here would be pinning the build this check exists to avoid.
        logLevel = "info,castaway=debug";
        settings = {
          friendly_name = friendlyName;
          uuid = configUuid;
          # The VM's default route is the NAT interface; pin discovery to the test LAN.
          interface = config.networking.primaryIPAddress;
          # The two surfaces under test — DIAL, and the DLNA root device it must not
          # collide with — pinned on; the rest off at *runtime* (the D55-sanctioned
          # switch) so a protocol this check makes no assertions about cannot take the
          # node down with it. `integration-vm` is where the rest of the surface runs.
          enable = {
            dial = true;
            dlna = true;
            cast = false;
            airplay = false;
            spotify = false;
            bluetooth = false;
            gamestream = false;
            matter = false;
            miracast = false;
          };
        };
      };

      systemd.services.castaway = {
        after = [ "xvfb.service" ];
        requires = [ "xvfb.service" ];
        environment = {
          DISPLAY = ":99";
          # The same software-Vulkan triplet as `checks.test` (see flake.nix `lavapipe`):
          # the kiosk's wgpu compositor needs an adapter and the VM has no GPU. The
          # loader itself is already on the kiosk wrapper's LD_LIBRARY_PATH.
          WGPU_BACKEND = "vulkan";
          VK_DRIVER_FILES =
            "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.${pkgs.stdenv.hostPlatform.parsed.cpu.name}.json";
          # Chromium wants a writable profile; a DynamicUser's home is `/`. The state
          # directory is the one writable place the module already owns.
          HOME = "%S/castaway";
        };
        serviceConfig = {
          # The X socket is /tmp/.X11-unix/X99; with PrivateTmp the kiosk sees an empty
          # /tmp and winit reports a display it cannot reach.
          PrivateTmp = lib.mkForce false;
          # Chromium's namespace sandbox clones user namespaces (the setuid helper
          # cannot exist in the store — see nix/electron-linux.nix, which chose the
          # namespace sandbox over `--no-sandbox` deliberately). RestrictNamespaces
          # would make the zygote abort and take the kiosk down with it.
          RestrictNamespaces = lib.mkForce false;
          # Appended, not forced — systemd unions repeated allowlist lines. The module's
          # `@system-service` set is missing three things Chromium needs, and a filtered
          # syscall under that set is a SIGSYS kill: V8 allocates memory-protection
          # keys (observed on this node as electron dying on syscall 330, `pkey_alloc`),
          # the namespace sandbox chroots its zygote into an empty directory (syscall
          # 161, `chroot` — the zygote host CHECK-crashes the whole browser when that
          # child dies), and the render sandbox installs its own seccomp/Landlock
          # filters (`@sandbox`). Verified natively: electron under `systemd-run` with
          # `@system-service` alone dies at the zygote; with these four lines it runs.
          SystemCallFilter = [ "@sandbox" "pkey_alloc" "pkey_free" "pkey_mprotect" "chroot" ];
        };
      };
    };

    # The sender: a plain host on the same LAN, walking the discovery path a real DIAL
    # sender walks.
    sender = { ... }: {
      # SSDP replies come back as unicast from :1900 to our ephemeral port, which
      # conntrack can't associate with a datagram sent to 239.255.255.250 — the default
      # firewall would drop every reply and fail the test for the wrong reason.
      networking.firewall.enable = false;
      environment.systemPackages = [ pkgs.curl ssdpSearch ];
    };
  };

  testScript = { nodes, ... }: ''
    import uuid as uuidlib

    kiosk = "${nodes.receiver.networking.primaryIPAddress}"
    base = f"http://{kiosk}:${toString httpPort}"
    lan = "${nodes.sender.networking.primaryIPAddress}"

    DIAL_ST = "urn:dial-multiscreen-org:service:dial:1"
    CFG_UUID = "${configUuid}"
    # `device_uuid(&config.uuid, "dial")`, computed independently: RFC 4122 v5 (SHA-1)
    # over the configured UUID as namespace. Python's uuid5 and Rust's Uuid::new_v5 are
    # both implementations of the same RFC, so this pins the derivation from outside.
    DIAL_UUID = str(uuidlib.uuid5(uuidlib.UUID(CFG_UUID), "dial"))


    def reply_headers(replies):
        """Parse `ssdp-search` output into one header dict per reply."""
        out = []
        for chunk in replies.split("--- reply from ")[1:]:
            headers = {}
            for line in chunk.splitlines()[1:]:
                if ":" in line:
                    key, value = line.split(":", 1)
                    headers[key.strip().upper()] = value.strip()
            out.append(headers)
        return out


    start_all()

    with subtest("the kiosk comes up under the module, with a browser to launch into"):
        receiver.wait_for_unit("castaway.service")
        receiver.wait_for_open_port(${toString httpPort})
        # DIAL is only advertised when there is a launch target (D27), so both halves
        # of that premise are asserted: the adapter said so, and the Electron
        # subprocess actually came up on the virtual display.
        receiver.wait_until_succeeds(
            "journalctl -u castaway --no-pager | grep -q 'enabled: DIAL'", timeout=120
        )
        receiver.wait_until_succeeds(
            "journalctl -u castaway --no-pager | grep -q 'browser up'", timeout=180
        )
        sender.wait_for_unit("multi-user.target")

    with subtest("a targeted DIAL M-SEARCH from another host draws DIAL's own reply"):
        replies = reply_headers(sender.succeed(f"ssdp-search '{DIAL_ST}' 5 {lan}"))
        # The DLNA device must not answer a search for a service it does not host —
        # and the reply that does come back is keyed to the *derived* uuid, ST echoed,
        # LOCATION pointing at the DIAL description.
        assert not any(
            r.get("USN", "").startswith(f"uuid:{CFG_UUID}") for r in replies
        ), replies
        ours = [r for r in replies if r.get("USN", "").startswith(f"uuid:{DIAL_UUID}")]
        assert len(ours) == 1, replies
        reply = ours[0]
        assert reply["ST"] == DIAL_ST, reply
        assert reply["USN"] == f"uuid:{DIAL_UUID}::{DIAL_ST}", reply
        assert reply["LOCATION"] == f"{base}/dial/dd.xml", reply

    with subtest("the LOCATION serves Application-URL and the UDN the search advertised"):
        headers = sender.succeed(f"curl -sSf -D - -o /tmp/dd.xml {base}/dial/dd.xml")
        app_url = next(
            line.split(":", 1)[1].strip()
            for line in headers.splitlines()
            if line.lower().startswith("application-url:")
        )
        # DIAL 2.1: the description response MUST carry Application-URL, and it is the
        # app base a sender appends the app name to.
        assert app_url == f"{base}/dial/apps/", headers
        body = sender.succeed("cat /tmp/dd.xml")
        # The UDN ties the description back to the SSDP USN — Chromium's DIAL parser
        # drops a device whose description carries none, and Android senders match the
        # two against each other.
        assert f"<UDN>uuid:{DIAL_UUID}</UDN>" in body, body
        # The per-protocol name suffix, seen from outside, as asserted for every other
        # surface in integration-vm.
        assert "<friendlyName>${friendlyName}#youtube</friendlyName>" in body, body

    with subtest("the Application-URL answers for the YouTube app"):
        state = sender.succeed(f"curl -sSf {app_url}YouTube")
        assert "<name>YouTube</name>" in state, state
        assert "<state>stopped</state>" in state, state

    with subtest("DIAL's root device does not collide with DLNA's on the wire"):
        # Both devices answer a root search; a control point keys its device table on
        # USN, so one USN with two LOCATIONs means one of the two descriptions is
        # dropped arbitrarily — the collision `device_uuid` exists to prevent, seen
        # from the other end of a real LAN.
        replies = reply_headers(sender.succeed(f"ssdp-search upnp:rootdevice 5 {lan}"))
        usns = {r["USN"] for r in replies}
        assert f"uuid:{CFG_UUID}::upnp:rootdevice" in usns, replies
        assert f"uuid:{DIAL_UUID}::upnp:rootdevice" in usns, replies

        # And across everything both devices advertise: one USN, one LOCATION.
        replies = reply_headers(sender.succeed(f"ssdp-search ssdp:all 5 {lan}"))
        seen = {}
        for r in replies:
            usn, location = r["USN"], r["LOCATION"]
            assert seen.setdefault(usn, location) == location, (
                f"USN {usn} answered with two LOCATIONs: {seen[usn]} and {location}"
            )
  '';
}
