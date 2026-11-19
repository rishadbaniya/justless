#import <Foundation/Foundation.h>

NS_ASSUME_NONNULL_BEGIN

@protocol SHPLedger <NSObject>
- (void)recordAmount:(NSInteger)amount;
@end

@interface SHPInvoiceService : NSObject
@property (nonatomic, copy) NSString *currency;
- (instancetype)initWithLedger:(id<SHPLedger>)ledger;
- (NSInteger)totalOfItems:(NSArray<NSNumber *> *)items;
+ (instancetype)serviceWithLedger:(id<SHPLedger>)ledger;
@end

NS_ASSUME_NONNULL_END
