package worker

// Carrier books parcels with a delivery partner.
type Carrier interface {
	BookPickup(sku string, count int) (string, error)
	CancelPickup(booking string) error
}
