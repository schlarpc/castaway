// Judges one device-auth vector the way a Cast sender would.
//
// Reads a vector directory — the peer (TLS) certificate, the DeviceAuthMessage the
// receiver answered with, the nonce the sender challenged with, the time to verify at,
// and optionally a trust anchor — and prints either `ok` or `error <Error::Code name>`.
// No policy of its own: every decision comes from openscreen's
// `AuthenticateChallengeReplyForTest`, which is `AuthenticateChallengeReply` with the
// clock and CRL policy made explicit so the verdict does not depend on the day it runs.
//
// Omitting `anchor.der` means the trust store senders actually ship — the Cast device
// roots — which is the case that decides whether an official sender will talk to us.
//
// See ../openscreen-device-auth.nix for how this is built, and the vectors' README for
// what each case is asking.

#include <cstdio>
#include <fstream>
#include <iostream>
#include <sstream>
#include <string>
#include <vector>

#include "cast/common/certificate/cast_cert_validator.h"
#include "cast/common/certificate/date_time.h"
#include "cast/common/channel/proto/cast_channel.pb.h"
#include "cast/common/public/parsed_certificate.h"
#include "cast/common/public/trust_store.h"
#include "cast/sender/channel/cast_auth_util.h"
#include "platform/base/error.h"

namespace {

std::string ReadFile(const std::string& path) {
  std::ifstream in(path, std::ios::binary);
  std::ostringstream ss;
  ss << in.rdbuf();
  return ss.str();
}

bool Exists(const std::string& path) {
  std::ifstream in(path, std::ios::binary);
  return in.good();
}

}  // namespace

int main(int argc, char** argv) {
  if (argc != 2) {
    std::cerr << "usage: oracle <case-dir>\n";
    return 2;
  }
  const std::string name = argv[1];
  const std::string dir = name + "/";

  const std::string peer_cert_der = ReadFile(dir + "peer_cert.der");
  const std::string auth_bin = ReadFile(dir + "auth.bin");
  const std::string nonce = Exists(dir + "nonce.bin") ? ReadFile(dir + "nonce.bin") : "";
  const int64_t when = std::stoll(ReadFile(dir + "time"));

  namespace oc = openscreen::cast;

  oc::proto::CastMessage msg;
  msg.set_protocol_version(oc::proto::CastMessage_ProtocolVersion_CASTV2_1_0);
  msg.set_source_id("receiver-0");
  msg.set_destination_id("sender-0");
  msg.set_namespace_("urn:x-cast:com.google.cast.tp.deviceauth");
  msg.set_payload_type(oc::proto::CastMessage_PayloadType_BINARY);
  msg.set_payload_binary(auth_bin);

  openscreen::ErrorOr<std::unique_ptr<oc::ParsedCertificate>> peer =
      oc::ParsedCertificate::ParseFromDER(
          openscreen::ByteView(
              reinterpret_cast<const uint8_t*>(peer_cert_der.data()),
              peer_cert_der.size()));
  if (!peer) {
    std::cout << "error PeerCertParse\n";
    return 0;
  }

  std::unique_ptr<oc::TrustStore> cast_trust;
  if (Exists(dir + "anchor.der")) {
    const std::string anchor = ReadFile(dir + "anchor.der");
    cast_trust = oc::TrustStore::CreateInstanceForTest(openscreen::ByteView(
        reinterpret_cast<const uint8_t*>(anchor.data()), anchor.size()));
  } else {
    cast_trust = oc::CastTrustStore::Create();
  }
  std::unique_ptr<oc::TrustStore> crl_trust = oc::CastCRLTrustStore::Create();

  openscreen::cast::DateTime verification_time = {};
  if (!oc::DateTimeFromSeconds(when, &verification_time)) {
    std::cerr << "bad time\n";
    return 2;
  }

  oc::AuthContext context = oc::AuthContext::CreateForTest(nonce);

  openscreen::ErrorOr<oc::CastDeviceCertPolicy> result =
      oc::AuthenticateChallengeReplyForTest(
          msg, *peer.value(), context, oc::CRLPolicy::kCrlOptional,
          cast_trust.get(), crl_trust.get(), verification_time);

  if (result.is_value()) {
    std::cout << "ok\n";
  } else {
    // openscreen prefixes every non-success code with "Failure: ". Drop it so the
    // recorded verdicts read as the code names they are.
    std::string code = openscreen::ToString(result.error().code());
    const std::string prefix = "Failure: ";
    if (code.rfind(prefix, 0) == 0) {
      code = code.substr(prefix.size());
    }
    std::cout << "error " << code << "\n";
    std::cerr << "  (" << name << ": " << result.error().message() << ")\n";
  }
  return 0;
}
