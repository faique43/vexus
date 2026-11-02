/** The contract every checkout provider adapter implements. */

export interface PaymentGateway {
    authorize(cents: number): Promise<string>;
    capture(authorizationId: string): Promise<void>;
    voidAuthorization(authorizationId: string): Promise<void>;
}
