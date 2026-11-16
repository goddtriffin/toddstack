import Foundation
import Testing
import ToddstackAccounts

@Suite struct PushTokenTests {
    @Test func tokenBytesBecomeLowercaseHexWithLeadingZerosKept() {
        let expected: String = "000fa0ff10"
        let actual: String = apnsTokenHex(Data([0x00, 0x0f, 0xa0, 0xff, 0x10]))
        #expect(expected == actual)
    }

    @Test func aThirtyTwoByteTokenIsSixtyFourCharacters() {
        #expect(64 == apnsTokenHex(Data(repeating: 0xab, count: 32)).count)
        #expect("" == apnsTokenHex(Data()))
    }
}
