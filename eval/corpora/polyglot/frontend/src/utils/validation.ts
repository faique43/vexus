/** Client-side input validation, mirrored (loosely) by the service's own checks. */

/** Check that an email address looks well-formed before submitting the form. */
export function isValidEmail(email: string): boolean {
    return /^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email);
}

/** Validate a cart line item's sku before it's added to the cart. */
export function validate(sku: string): boolean {
    return sku.length > 0 && sku.startsWith("sku-");
}
