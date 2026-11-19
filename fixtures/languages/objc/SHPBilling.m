#import "SHPBilling.h"
#import <Foundation/Foundation.h>

static NSInteger SHPApplyTax(NSInteger amount) {
    return amount * 2;
}

@interface SHPInvoiceService ()
@property (nonatomic, strong) id<SHPLedger> ledger;
@end

@implementation SHPInvoiceService

- (instancetype)initWithLedger:(id<SHPLedger>)ledger {
    self = [super init];
    if (self) {
        _ledger = ledger;
    }
    return self;
}

- (NSInteger)totalOfItems:(NSArray<NSNumber *> *)items {
    NSInteger sum = 0;
    for (NSNumber *item in items) {
        sum += item.integerValue;
    }
    [self.ledger recordAmount:sum];
    return SHPApplyTax(sum);
}

+ (instancetype)serviceWithLedger:(id<SHPLedger>)ledger {
    return [[self alloc] initWithLedger:ledger];
}

@end
