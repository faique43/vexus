/* Order-payload checksums for the native point-of-sale bridge. */
#include <stddef.h>

/* One step of the Fletcher-16 rolling sums. */
static void fletcher_step(unsigned int *sum1, unsigned int *sum2, unsigned char byte) {
    *sum1 = (*sum1 + byte) % 255;
    *sum2 = (*sum2 + *sum1) % 255;
}

/* Fletcher-16 checksum over an order payload, as sent by the POS bridge. */
unsigned int order_checksum(const unsigned char *data, size_t len) {
    unsigned int sum1 = 0;
    unsigned int sum2 = 0;
    for (size_t i = 0; i < len; i++) {
        fletcher_step(&sum1, &sum2, data[i]);
    }
    return (sum2 << 8) | sum1;
}
