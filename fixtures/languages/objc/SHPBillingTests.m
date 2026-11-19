#import <XCTest/XCTest.h>
#import "SHPBilling.h"

@interface SHPBillingTests : XCTestCase
@end

@implementation SHPBillingTests

- (void)testTotalsItems {
    SHPInvoiceService *service = [SHPInvoiceService serviceWithLedger:nil];
    XCTAssertEqual([service totalOfItems:@[@1, @2]], 6);
}

@end
