#!/usr/bin/env bash
# Regenerate the TLS test fixtures in this directory.
#
# These are THROWAWAY certificates for `crates/jean-model`'s TLS tests and nothing else. They are
# never trusted by Windows, never imported into any store, and never used to reach a network:
# the tests hand the CA DER to a single `ureq` agent as an isolated anchor set. Do not copy them
# into a real configuration, and do not treat "the handshake succeeded" as evidence about any
# certificate that exists outside this directory.
#
# Requires: openssl 3.x (verified with 3.5.9). RSA-2048 + SHA-256 + PKCS#8 keys, and a PKCS#12
# bundle of each leaf for the test server to import.
# Validity is 5000 days (~2040); a TLS test that suddenly starts failing on the accepted rows is
# most likely looking at an expired fixture, and `openssl x509 -in <file> -noout -enddate` says so.
#
# Usage:  ./generate.sh          (run from this directory)
set -euo pipefail
cd "$(dirname "$0")"

# Git Bash rewrites a leading-slash argument like `/O=JEAN test/CN=...` into a Windows path before
# openssl sees it, and the subject then fails to parse. The variable is simply unused on other
# platforms, so setting it unconditionally keeps this script one command on every machine.
export MSYS2_ARG_CONV_EXCL='*'

DAYS=5000
# Not a secret: it protects a throwaway test key, and it appears in the test that reads the file.
PFX_PASSWORD=jean-test
SUBJ_CA="/O=JEAN test/CN=JEAN test CA"
SUBJ_LEAF="/O=JEAN test/CN=JEAN test server"

gen_ca() { # $1 = name suffix
    openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "ca-$1.key.pem"
    openssl req -x509 -new -key "ca-$1.key.pem" -sha256 -days "$DAYS" \
        -subj "$SUBJ_CA $1" \
        -addext "basicConstraints=critical,CA:TRUE" \
        -addext "keyUsage=critical,keyCertSign,cRLSign" \
        -out "ca-$1.cert.pem"
}

gen_leaf() { # $1 = file stem, $2 = DNS subjectAltName, $3 = CA suffix
    openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$1.key.pem"
    openssl req -new -key "$1.key.pem" -subj "$SUBJ_LEAF" -out "$1.csr"
    # Every extension comes from the file below. The CSR carries none, so there is no second
    # definition of subjectAltName for openssl to either drop or reject as a duplicate.
    # A real file, not `<(...)`  — the Windows build of openssl cannot open a /dev/fd path.
    printf 'keyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:%s\n' "$2" > "$1.ext"
    openssl x509 -req -in "$1.csr" -CA "ca-$3.cert.pem" -CAkey "ca-$3.key.pem" \
        -days "$DAYS" -sha256 -CAcreateserial -extfile "$1.ext" -out "$1.cert.pem"
    # The server identity for the tests. `native-tls`'s Windows `Identity::from_pkcs8` associates
    # the key with `KeySpec::key_exchange()`, and Schannel then refuses the credential outright
    # (SEC_E_NO_CREDENTIALS, os error -2146893042) at accept — measured on this machine, not
    # theorised. PKCS#12 is the shape Windows imports cleanly. The SHA1/3DES PBEs are the ones
    # CryptoAPI has always supported; OpenSSL 3.x's AES-PBES2 defaults were not tested here, so the
    # compatible choice is deliberate rather than discovered.
    openssl pkcs12 -export -name "JEAN test server" \
        -inkey "$1.key.pem" -in "$1.cert.pem" -certfile "ca-$3.cert.pem" \
        -macalg SHA1 -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES \
        -passout pass:"$PFX_PASSWORD" -out "$1.pfx"
    # The server identity presents leaf then its issuer, so the handshake has a chain to build on.
    cat "$1.cert.pem" "ca-$3.cert.pem" > "$1.chain.pem"
    rm -f "$1.csr" "$1.ext"
}

gen_ca a
gen_ca b
# Trusted name: issued by CA A, valid for `localhost`.
gen_leaf leaf-localhost localhost a
# Mismatched name: issued by the SAME CA A, so the chain is trusted and only the name is wrong.
gen_leaf leaf-other-host other.invalid a
# Untrusted: issued by CA B, which no test ever adds to its anchor set.
gen_leaf leaf-untrusted-ca-b localhost b

for ca in a b; do
    openssl x509 -in "ca-$ca.cert.pem" -outform DER -out "ca-$ca.der"
done
rm -f ca-a.cert.srl ca-b.cert.srl

echo
echo "Generated. Every one of these is a test-only artefact; the keys and the .pfx bundles belong"
echo "to certificates no store on this machine trusts, and must never be pointed at by a real"
echo "configuration:"
ls -1 *.pem *.der *.pfx
echo
echo "Validity of each certificate:"
for f in ca-a.cert.pem ca-b.cert.pem leaf-localhost.cert.pem leaf-other-host.cert.pem leaf-untrusted-ca-b.cert.pem; do
    printf '  %-32s ' "$f"
    openssl x509 -in "$f" -noout -enddate -subject | tr '\n' ' '
    printf '\n'
done
