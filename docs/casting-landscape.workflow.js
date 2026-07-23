export const meta = {
  name: 'casting-protocol-landscape',
  description: 'Map screen-mirroring / casting / streaming-receiver protocols across platforms (receiver-focused), cross-validate contested claims, synthesize a landscape report',
  phases: [
    { title: 'Research', detail: 'One agent per protocol family, web-researched structured dossier' },
    { title: 'Cross-validate', detail: 'Adversarial verification of contested/uncertain claims' },
    { title: 'Synthesize', detail: 'Assemble the landscape map and scope-of-work notes' },
  ],
}

const PROTOCOLS = [
  { key: 'google-cast', title: 'Google Cast / Chromecast', focus: 'Clarify precisely the relationship between Chromecast (the device/brand), Google Cast (the SDK/protocol), and Chromecast built-in. Cover the CASTv2 protocol, the Web Receiver / Cast Application Framework (CAF), sender apps, the mDNS plus protobuf-over-TLS control channel, media delivery (HLS/DASH/MP4 by URL), device auth. Note the Chromecast device EOL (2024) and Google TV Streamer.' },
  { key: 'airplay', title: 'AirPlay (1 and 2, mirroring/audio/video)', focus: 'Distinguish AirPlay 1 vs AirPlay 2; screen mirroring vs video (AV) streaming vs audio streaming. Cover discovery (Bonjour/mDNS), the RAOP audio path, mirroring path (h264/HEVC over a proprietary session), pairing/auth (SRP, MFi, HomeKit for AirPlay 2 audio), FairPlay. Note reverse-engineered receivers.' },
  { key: 'dial', title: 'DIAL (Discovery and Launch)', focus: 'What DIAL actually is (app launch, NOT streaming), SSDP discovery plus REST app API, who authored it (Netflix/YouTube), its relationship to Chromecast and to smart-TV YouTube/Netflix cast buttons. Auth/pairing model.' },
  { key: 'miracast-widi', title: 'Miracast and Intel WiDi', focus: 'Wi-Fi Alliance Miracast: Wi-Fi Direct (P2P) transport, WFD, H.264 over RTP/MPEG-TS, HDCP 2.x, WPS pairing. History of Intel WiDi and how it folded into Miracast. Certification/licensing. Windows Connect / Wireless Display receiver. Open implementations (MiracleCast).' },
  { key: 'dlna-upnp', title: 'DLNA / UPnP AV', focus: 'UPnP AV device model (MediaServer, MediaRenderer, ControlPoint), SSDP discovery, SOAP control, AVTransport/RenderingControl, push vs pull content, DIDL-Lite, DTCP-IP link protection. DLNA guidelines vs raw UPnP. Open implementations (gmediarender, gerbera/mediatomb, Rygel). Note DLNA org dissolution.' },
  { key: 'carplay-wireless', title: 'Wireless CarPlay', focus: 'How CarPlay works: it rides on AirPlay screen mirroring plus iAP2/accessory protocols, Wi-Fi plus Bluetooth bootstrap, MFi authentication chip requirement. Receiver = head unit. Reverse-engineered receiver projects (OpenAuto/react-carplay/carlinkit dongles). Licensing barrier.' },
  { key: 'android-auto-wireless', title: 'Wireless Android Auto', focus: 'AA protocol: projection over USB (AOAP) or wireless (Wi-Fi plus BT bootstrap), the protobuf-based channel, video H.264, audio, touch/input. Head-unit certification. Open projects (openauto/aasdk/headunit reversing). Contrast auth model with CarPlay.' },
  { key: 'samsung', title: 'Samsung (Smart View / Quick Share / Tap View)', focus: 'What Samsung layers on top of standards: Smart View app, screen mirroring via Miracast, DLNA, plus proprietary discovery. Quick Share, Tap View, and any Samsung-specific casting. How much is standard vs proprietary.' },
  { key: 'xiaomi-huawei', title: 'Xiaomi / Huawei / other Android OEM casting', focus: 'Xiaomi Mi Screen/Cast, Huawei Cast+/Cast, and generic Android wireless display. How much is Miracast/Wi-Fi Direct vs proprietary extensions. Huawei Cast+ as a Miracast alternative. HONOR/OPPO/vivo variants briefly.' },
  { key: 'spotify-connect', title: 'Spotify Connect', focus: 'App-specific casting: how Spotify Connect works (control handoff, cloud-mediated, device plays its own stream), discovery (mDNS zeroconf plus cloud), the closed protocol (librespot reverse-engineering), the licensed eSDK for hardware makers. Auth (Spotify account tokens, blob).' },
  { key: 'sonos', title: 'Sonos', focus: 'Sonos control/streaming architecture: UPnP underpinnings historically, the S2 platform, local UPnP control API plus newer cloud API, Sonos as a DLNA renderer, AirPlay 2 and Spotify Connect support. What is open vs proprietary. SMAPI for music service integration.' },
  { key: 'matter-casting', title: 'Matter Casting', focus: 'The new CSA Matter Casting spec: goals (unify casting across ecosystems), relationship to Cast and AirPlay, how it does discovery/commissioning (Matter fabric), content launch model. Maturity/adoption as of 2025-2026. Is it a real receiver protocol yet.' },
  { key: 'roku-fire-whisperplay', title: 'Roku ECP and Amazon Fling/Whisperplay', focus: 'Roku External Control Protocol (ECP, SSDP plus REST), Roku as DIAL target, Roku screen mirroring via Miracast. Amazon Whisperplay / Fling SDK, Fire TV mirroring (Miracast), Amazon DIAL. App-launch vs media-fling vs mirroring distinctions.' },
  { key: 'webrtc-browser', title: 'WebRTC and browser-based casting', focus: 'Presentation API / Remote Playback API in browsers, Chrome tab/desktop casting (uses Cast plus WebRTC/RTP for mirroring), WebRTC as a transport for low-latency screen sharing, how this differs from media-URL casting. Open Screen Protocol (W3C/OpenScreen) as a possible successor to Cast/DIAL.' },
  { key: 'transport-layer', title: 'Lower-level transport and media substrate', focus: 'The shared substrate protocols the above ride on: mDNS/DNS-SD (Bonjour), SSDP/UPnP, Wi-Fi Direct/P2P, RTSP/RTP/RTCP, MPEG-TS, HLS/DASH progressive delivery, TLS/DTLS, SRTP, codecs (H.264/HEVC/AV1, AAC/Opus/ALAC), and link protection (HDCP, DTCP-IP, FairPlay). Map which protocol families depend on which substrate.' },
]

const DOSSIER_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['name', 'category', 'summary', 'transmitterPlatforms', 'receiverPlatforms', 'howItWorks', 'lowerLevelProtocols', 'authEncryption', 'openImplementations', 'closedImplementations', 'protocolDocs', 'receiverFeasibility', 'transmitterNotes', 'contestedClaims', 'confidence'],
  properties: {
    name: { type: 'string' },
    aka: { type: 'array', items: { type: 'string' } },
    category: { type: 'string', description: 'e.g. screen-mirroring, media-URL-casting, app-launch, app-specific-handoff, automotive-projection, transport-substrate' },
    summary: { type: 'string', description: '2-4 sentence plain-language description of what it is and does' },
    variants: { type: 'array', items: { type: 'string' }, description: 'named versions/variants and how they differ' },
    transmitterPlatforms: { type: 'array', items: { type: 'string' }, description: 'which platforms can act as sender/source and how (native vs app vs SDK)' },
    receiverPlatforms: { type: 'array', items: { type: 'string' }, description: 'which platforms/devices can act as receiver/sink and how' },
    howItWorks: { type: 'string', description: 'discovery, pairing/auth, session setup, streaming, at a shape level' },
    lowerLevelProtocols: { type: 'array', items: { type: 'string' }, description: 'transport/media protocols it rides on (mDNS, SSDP, RTP, TLS, HLS, Wi-Fi Direct, etc.)' },
    authEncryption: { type: 'string', description: 'pairing, authentication, DRM/link-protection, encryption specifics' },
    openImplementations: { type: 'array', items: { type: 'object', additionalProperties: false, required: ['name', 'side', 'notes'], properties: { name: { type: 'string' }, side: { type: 'string', description: 'receiver, transmitter, or both' }, language: { type: 'string' }, maturity: { type: 'string' }, notes: { type: 'string' }, url: { type: 'string' } } } },
    closedImplementations: { type: 'array', items: { type: 'object', additionalProperties: false, required: ['name', 'notes'], properties: { name: { type: 'string' }, licensing: { type: 'string', description: 'licensing/certification program, cost/NDA barriers if known' }, notes: { type: 'string' } } } },
    protocolDocs: { type: 'string', description: 'availability of specs: official/public, licensed-under-NDA, reverse-engineered-only, none. Name the key docs/reverse-eng writeups.' },
    receiverFeasibility: { type: 'string', description: 'RECEIVER-SIDE focus: how hard is it to build an open receiver, what are the blockers (auth chips, DRM, closed specs)' },
    transmitterNotes: { type: 'string', description: 'transmitter-side notes and feasibility' },
    contestedClaims: { type: 'array', description: 'claims in this dossier you are least sure about or that are commonly confused/misreported', items: { type: 'object', additionalProperties: false, required: ['claim', 'whyUncertain'], properties: { claim: { type: 'string' }, whyUncertain: { type: 'string' } } } },
    confidence: { type: 'string' },
  },
}

const VERIFY_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['claim', 'verdict', 'evidence'],
  properties: {
    claim: { type: 'string' },
    verdict: { type: 'string', description: 'confirmed | refuted | nuanced | uncertain' },
    evidence: { type: 'string', description: 'what independent sources say; correct the claim if wrong' },
    sources: { type: 'array', items: { type: 'string' } },
  },
}

const researchPrompt = (p) => [
  'You are a protocol researcher building a technical dossier on: ' + p.title + '.',
  '',
  'Focus instructions: ' + p.focus,
  '',
  'Use web search and page-fetch tools (load them via ToolSearch: query "select:WebSearch,WebFetch") to research from MULTIPLE independent sources: official docs, standards bodies, reverse-engineering writeups (GitHub projects, blog teardowns), and Wikipedia for orientation. Cross-check facts across at least 2 sources before asserting them. Prefer primary/technical sources over marketing.',
  '',
  'The audience is a systems engineer trying to understand the SHAPE and SCOPE of building an open RECEIVER (sink) for these protocols, with secondary interest in the transmitter side. Do not go into byte-level detail; give enough that they understand the architecture, the auth/DRM blockers, and what open/closed implementations already exist. Be precise about commonly-confused distinctions.',
  '',
  'Return the structured dossier. In contestedClaims, flag anything you are genuinely unsure about or that is commonly misreported: these get independently verified.',
].join('\n')

const verifyPrompt = (f) => [
  'Independently fact-check this claim about ' + f.protocol + '. Be adversarial: try to find sources that CONTRADICT it, and default to nuanced or refuted if the reality is more complex than stated.',
  '',
  'CLAIM: ' + f.claim,
  'Why it was flagged as uncertain: ' + f.whyUncertain,
  '',
  'Use WebSearch/WebFetch (load via ToolSearch "select:WebSearch,WebFetch"). Consult sources INDEPENDENT of whatever the original claim likely came from. Return your verdict.',
].join('\n')

const synthPrompt = (dossiers, verifications) => [
  'You are writing a landscape/scope report on streaming-receiver, screen-mirroring, and content-casting protocols for a systems engineer. Your job is to give them the SHAPE of the thing and the SCOPE of building an open receiver, not exhaustive detail.',
  '',
  'You are given (1) structured dossiers per protocol family and (2) independent cross-validation verdicts on contested claims. Where a verdict corrects or nuances a dossier, TRUST THE VERDICT and reflect the corrected version.',
  '',
  'Produce a well-organized markdown report with these sections:',
  '1. Taxonomy: the major CATEGORIES of these protocols (e.g. media-URL casting vs pixel screen-mirroring vs app-launch/DIAL vs app-specific control-handoff vs automotive projection), with which protocols fall in each. This is the most important framing.',
  '2. The shared substrate: the lower-level protocols everything rides on, and a compact mapping of which protocol depends on which.',
  '3. Per-protocol rundown: grouped by category, each entry compact: what it is, transmitter/receiver platform support, auth/encryption/DRM, open implementations, closed/licensed implementations plus the barrier, doc availability, and RECEIVER-side feasibility (the headline: how hard/blocked).',
  '4. Receiver-side scope assessment: a tiered view: which protocols are realistically buildable open (specs public / good reverse-eng), which are hard-but-done (reverse-engineered, fragile), which are effectively locked (auth chips, DRM, NDA). Call out the specific blockers (MFi/FairPlay, HDCP, protobuf-over-TLS device auth, cloud dependency, etc.).',
  '5. Cross-cutting notes: commonly-confused distinctions clarified (esp. Google Cast vs Chromecast vs Chromecast-built-in; Google Cast vs DIAL; Miracast vs WiDi; AirPlay variants), and notable trends (Chromecast device EOL, Matter Casting, Open Screen Protocol, DLNA org dissolution).',
  '',
  'Be concrete and name real projects/specs. Keep prose tight: this reader values density over hand-holding. Use tables where they help.',
  '',
  '=== DOSSIERS (JSON) ===',
  JSON.stringify(dossiers),
  '',
  '=== CROSS-VALIDATION VERDICTS (JSON) ===',
  JSON.stringify(verifications),
].join('\n')

phase('Research')
const dossiers = (await parallel(PROTOCOLS.map(p => () =>
  agent(researchPrompt(p), { label: 'research:' + p.key, phase: 'Research', schema: DOSSIER_SCHEMA })
))).filter(Boolean)

phase('Cross-validate')
const flagged = dossiers.flatMap(d => (d.contestedClaims || []).slice(0, 2).map(c => ({ protocol: d.name, claim: c.claim, whyUncertain: c.whyUncertain })))
const toVerify = flagged.slice(0, 14)
log(dossiers.length + ' dossiers collected; cross-validating ' + toVerify.length + ' contested claims')
const verifications = (await parallel(toVerify.map(f => () =>
  agent(verifyPrompt(f), { label: 'verify:' + f.protocol.slice(0, 18), phase: 'Cross-validate', schema: VERIFY_SCHEMA })
))).filter(Boolean)

phase('Synthesize')
const report = await agent(synthPrompt(dossiers, verifications), { label: 'synthesize', phase: 'Synthesize' })

return { report, dossierCount: dossiers.length, verificationCount: verifications.length, verifications, dossiers }
