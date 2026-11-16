import Foundation
import Testing
import ToddstackAccounts

@Suite struct PkceTests {
    @Test func rfc7636AppendixBVectorsMatch() {
        let octets: [UInt8] = [116, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212, 37, 77, 105, 214, 191, 240, 91, 88, 5, 88, 83, 132, 141, 121]
        let expectedVerifier: String = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        let expectedChallenge: String = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        #expect(expectedVerifier == base64URL(Data(octets)))
        #expect(expectedChallenge == pkceChallenge(verifier: expectedVerifier))
        #expect(PkcePair(verifier: expectedVerifier) == PkcePair(verifier: expectedVerifier))
        #expect(expectedChallenge == PkcePair(verifier: expectedVerifier).challenge)
    }

    @Test func generatedVerifierIsFortyThreeUnpaddedBase64urlCharactersFromThirtyTwoBytes() {
        let pair: PkcePair = PkcePair.generate()
        #expect(43 == pair.verifier.count)
        #expect(!pair.verifier.contains { "+/=".contains($0) })
        #expect(pair.challenge == pkceChallenge(verifier: pair.verifier))
        #expect(pair != PkcePair.generate())
    }

    @Test func constantTimeEqualsComparesByValueAndLength() {
        #expect(constantTimeEquals("abc", "abc"))
        #expect(!constantTimeEquals("abc", "abd"))
        #expect(!constantTimeEquals("abc", "abcd"))
        #expect(!constantTimeEquals("", "a"))
        #expect(constantTimeEquals("", ""))
    }
}
