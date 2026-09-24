#ifndef _IPXE_VIRTIO_H
#define _IPXE_VIRTIO_H

/** @file
 *
 * Virtual I/O device
 *
 */

FILE_LICENCE ( GPL2_OR_LATER_OR_UBDL );
FILE_SECBOOT ( PERMITTED );

#include <stdint.h>
#include <byteswap.h>
#include <ipxe/dma.h>
#include <ipxe/pci.h>

/** Virtio page alignment */
#define VIRTIO_PAGE 4096

/** Maximum time to wait for reset (in ms) */
#define VIRTIO_RESET_MAX_WAIT_MS 100

/**
 * @defgroup virtio_legacy Original ("legacy") common device registers
 * @{
 */

/** Legacy device supported features register */
#define VIRTIO_LEG_FEAT 0x00

/** Legacy negotiated in-use features register */
#define VIRTIO_LEG_USED 0x04

/** Legacy queue base address register */
#define VIRTIO_LEG_BASE 0x08

/** Legacy queue size register */
#define VIRTIO_LEG_SIZE 0x0c

/** Legacy queue select register */
#define VIRTIO_LEG_SEL 0x0e

/** Legacy queue doorbell notification register */
#define VIRTIO_LEG_DB 0x10

/** Legacy driver status register */
#define VIRTIO_LEG_STAT 0x12
#define VIRTIO_STAT_ACKNOWLEDGE	0x0001	/**< Guest has found device */
#define VIRTIO_STAT_DRIVER	0x0002	/**< Guest driver exists */
#define VIRTIO_STAT_DRIVER_OK	0x0004	/**< Guest driver is ready */
#define VIRTIO_STAT_FEATURES_OK	0x0008	/**< Guest driver has set features */
#define VIRTIO_STAT_FAIL	0x0080	/**< Guest driver has failed */

/** Legacy device-specific registers */
#define VIRTIO_LEG_DEV 0x14

/** Legacy device-specific register (if MSI-X is enabled) */
#define VIRTIO_LEG_DEV_MSIX 0x18

/** @} */

/**
 * @defgroup virtio_pci_cap PCI capability registers
 * @{
 */

/** Capability type */
#define VIRTIO_PCI_CAP_TYPE 0x03
#define VIRTIO_PCI_CAP_TYPE_COMMON 0x01	/**< Common registers */
#define VIRTIO_PCI_CAP_TYPE_NOTIFY 0x02	/**< Notification doorbells */
#define VIRTIO_PCI_CAP_TYPE_DEVICE 0x04	/**< Device-specific registers */

/** Capability BAR index */
#define VIRTIO_PCI_CAP_BAR 0x04

/** Capability BAR offset */
#define VIRTIO_PCI_CAP_OFFSET 0x08

/** Capability minimum length */
#define VIRTIO_PCI_CAP_END 0x10

/** Notification doorbell capability multiplier offset */
#define VIRTIO_PCI_CAP_NOTIFY_MULT 0x10

/** Notification doorbell capability minimum length */
#define VIRTIO_PCI_CAP_NOTIFY_END 0x14

/** @} */

/** A virtio PCI capability */
struct virtio_pci_capability {
	/** Capability type */
	uint8_t type;
	/** Capability offset */
	uint8_t pos;
	/** Capability length */
	uint8_t len;
	/** BAR number */
	uint8_t bar;
	/** Offset within BAR */
	uint32_t offset;
};

/**
 * @defgroup virtio_pci_common PCI common device registers
 * @{
 */

/** PCI device supported features select register */
#define VIRTIO_PCI_FEAT_SEL 0x00

/** PCI device supported features register */
#define VIRTIO_PCI_FEAT 0x04

/** PCI negotiated in-use features select register */
#define VIRTIO_PCI_USED_SEL 0x08

/** PCI negotiated in-use features register */
#define VIRTIO_PCI_USED 0x0c

/** PCI device status register */
#define VIRTIO_PCI_STAT 0x14

/** PCI configuration generation register */
#define VIRTIO_PCI_GEN 0x15

/** PCI queue select register */
#define VIRTIO_PCI_SEL 0x16

/** PCI queue size register */
#define VIRTIO_PCI_SIZE 0x18

/** PCI queue enable register */
#define VIRTIO_PCI_ENABLE 0x1c

/** PCI queue doorbell notification offset register */
#define VIRTIO_PCI_DBOFF 0x1e

/** PCI queue descriptor array base address register */
#define VIRTIO_PCI_DESC 0x20

/** PCI queue submission queue base address register */
#define VIRTIO_PCI_SQ 0x28

/** PCI queue completion queue base address register */
#define VIRTIO_PCI_CQ 0x30

/** @} */

/** A virtio buffer descriptor */
struct virtio_desc {
	/** Buffer address */
	uint64_t addr;
	/** Buffer length */
	uint32_t len;
	/** Flags */
	uint16_t flags;
	/** Next descriptor index */
	uint16_t next;
} __attribute__ (( packed ));

/** Next descriptor index is valid */
#define VIRTIO_DESC_FL_NEXT 0x0001

/** Buffer is write-only */
#define VIRTIO_DESC_FL_WRITE 0x0002

/** A virtio submission queue entry */
struct virtio_sqe {
	/** Starting descriptor index */
	uint16_t index;
} __attribute__ (( packed ));

/** A virtio submission ("available") queue */
struct virtio_sq {
	/** Flags */
	uint16_t flags;
	/** Producer index */
	uint16_t prod;
	/** Queue entries */
	struct virtio_sqe sqe[];
} __attribute__ (( packed ));

/** Do not generate interrupt */
#define VIRTIO_SQ_FL_NO_INTERRUPT 0x0001

/** A virtio completion queue entry */
struct virtio_cqe {
	/** Starting descriptor index */
	uint32_t index;
	/** Length written */
	uint32_t len;
} __attribute__ (( packed ));

/** A virtio completion ("used") queue */
struct virtio_cq {
	/** Flags */
	uint16_t flags;
	/** Producer index */
	uint16_t prod;
	/** Queue entries */
	struct virtio_cqe cqe[];
} __attribute__ (( packed ));

/** A virtio queue */
struct virtio_queue {
	/** Queue index */
	unsigned int index;
	/** Queue size (must be a power of two) */
	unsigned int count;
	/** Queue mask */
	unsigned int mask;
	/** Submission queue producer index */
	unsigned int prod;
	/** Completion queue consumer index */
	unsigned int cons;
	/** Total length of queue */
	size_t len;
	/** DMA mapping */
	struct dma_mapping map;
	/** Descriptor array (and start of DMA allocation) */
	struct virtio_desc *desc;
	/** Submission queue */
	struct virtio_sq *sq;
	/** Completion queue */
	struct virtio_cq *cq;
	/** Notification doorbell */
	void *db;
};

/**
 * Initialise virtio queue
 *
 * @v queue		Virtio queue
 * @v index		Queue index
 */
static inline __attribute__ (( always_inline )) void
virtio_queue_init ( struct virtio_queue *queue, unsigned int index ) {

	queue->index = index;
}

/**
 * Calculate aligned size
 *
 * @v size		Unaligned size
 * @ret size		Aligned size
 */
static inline __attribute__ (( always_inline )) size_t
virtio_align ( size_t size ) {

	return ( ( size + VIRTIO_PAGE - 1 ) & ~( VIRTIO_PAGE - 1 ) );
}

/**
 * Calculate (unaligned) descriptor array size
 *
 * @v queue		Virtio queue
 * @v count		Queue size
 */
static inline __attribute__ (( always_inline )) size_t
virtio_desc_size ( unsigned int count ) {
	struct virtio_desc *desc;

	return ( count * sizeof ( desc[0] ) );
}

/**
 * Calculate (unaligned) submission queue size
 *
 * @v queue		Virtio queue
 * @v count		Queue size
 */
static inline __attribute__ (( always_inline )) size_t
virtio_sq_size ( unsigned int count ) {
	struct virtio_sq *sq;

	return ( sizeof ( *sq ) + ( count * sizeof ( sq->sqe[0] ) ) );
}

/**
 * Calculate (unaligned) completion queue size
 *
 * @v queue		Virtio queue
 * @v count		Queue size
 */
static inline __attribute__ (( always_inline )) size_t
virtio_cq_size ( unsigned int count ) {
	struct virtio_cq *cq;

	return ( sizeof ( *cq ) + ( count * sizeof ( cq->cqe[0] ) ) );
}

/** Number of 32-bit feature words */
#define VIRTIO_FEATURE_WORDS 2

/** A virtio feature set */
struct virtio_features {
	/** Feature words */
	uint32_t word[VIRTIO_FEATURE_WORDS];
};

/** Arbitrary descriptor layouts may be used */
#define VIRTIO_FEAT0_ANY_LAYOUT 0x08000000

/** Virtio version 1.0 or above */
#define VIRTIO_FEAT1_MODERN 0x00000001

/** A virtio device */
struct virtio_device {
	/** Device name */
	const char *name;
	/** Device operations */
	struct virtio_operations *op;
	/** DMA device */
	struct dma_device *dma;
	/** Common registers */
	void *common;
	/** Doorbell notification registers */
	void *notify;
	/** Device-specific registers */
	void *device;
	/** Driver status */
	unsigned int stat;
	/** Device supported features */
	struct virtio_features supported;
	/** Negotiated features */
	struct virtio_features features;
	/** Notification doorbell multiplier */
	unsigned int multiplier;
};

/** Virtio device operations */
struct virtio_operations {
	/**
	 * Reset device
	 *
	 * @v virtio		Virtio device
	 * @ret rc		Return status code
	 */
	int ( * reset ) ( struct virtio_device *virtio );
	/**
	 * Report driver status
	 *
	 * @v virtio		Virtio device
	 * @ret stat		Actual device status
	 */
	unsigned int ( * status ) ( struct virtio_device *virtio );
	/**
	 * Get supported features
	 *
	 * @v virtio		Virtio device
	 */
	void ( * supported ) ( struct virtio_device *virtio );
	/**
	 * Set negotiated features
	 *
	 * @v virtio		Virtio device
	 */
	void ( * negotiate ) ( struct virtio_device *virtio );
	/**
	 * Set queue size
	 *
	 * @v virtio		Virtio device
	 * @v queue		Virtio queue
	 * @v count		Requested size
	 */
	void ( * size ) ( struct virtio_device *virtio,
			  struct virtio_queue *queue, unsigned int count );
	/**
	 * Enable queue
	 *
	 * @v virtio		Virtio device
	 * @v queue		Virtio queue
	 */
	void ( * enable ) ( struct virtio_device *virtio,
			    struct virtio_queue *queue );
};

/**
 * Submit descriptor(s) to queue
 *
 * @v queue		Virtio queue
 * @v index		Starting descriptor index
 */
static inline __attribute__ (( always_inline )) void
virtio_submit ( struct virtio_queue *queue, unsigned int index ) {
	struct virtio_sqe *sqe;

	/* Get next submission queue entry */
	sqe = &queue->sq->sqe[ queue->prod++ & queue->mask ];

	/* Populate submission queue entry */
	sqe->index = cpu_to_le16 ( index );
}

/**
 * Notify queue
 *
 * @v queue		Virtio queue
 */
static inline __attribute__ (( always_inline )) void
virtio_notify ( struct virtio_queue *queue ) {

	/* Write producer index */
	wmb();
	queue->sq->prod = cpu_to_le16 ( queue->prod );
	wmb();

	/* Ring doorbell */
	iowrite16 ( queue->index, queue->db );
}

/**
 * Check for completed descriptors
 *
 * @v queue		Virtio queue
 * @v completions	Number of pending completions
 */
static inline __attribute__ (( always_inline )) unsigned int
virtio_completions ( struct virtio_queue *queue ) {
	uint16_t completions;

	/* Get completion count */
	completions = ( le16_to_cpu ( queue->cq->prod ) - queue->cons );
	return completions;
}

/**
 * Complete descriptor(s)
 *
 * @v queue		Virtio queue
 * @v len		Length to fill in, or NULL
 * @ret index		Starting descriptor index
 */
static inline __attribute__ (( always_inline )) unsigned int
virtio_complete ( struct virtio_queue *queue, size_t *len ) {
	struct virtio_cqe *cqe;

	/* Get next completion queue entry */
	cqe = &queue->cq->cqe[ queue->cons++ & queue->mask ];

	/* Parse completion queue entry */
	if ( len )
		*len = le32_to_cpu ( cqe->len );
	return le32_to_cpu ( cqe->index );
}

/**
 * Check if device is using the legacy interface
 *
 * @v virtio		Virtio device
 * @ret is_legacy	Device is using the legacy interface
 */
static inline __attribute__ (( always_inline )) int
virtio_is_legacy ( struct virtio_device *virtio ) {

	/* Check negotiation of version 1.0 or above */
	return ( ! ( virtio->features.word[1] & VIRTIO_FEAT1_MODERN ) );
}

extern int virtio_pci_map ( struct virtio_device *virtio,
			    struct pci_device *pci );
extern int virtio_reset ( struct virtio_device *virtio );
extern unsigned int virtio_status ( struct virtio_device *virtio,
				    unsigned int stat );
extern int virtio_init ( struct virtio_device *virtio,
			 const struct virtio_features *driver );
extern int virtio_enable ( struct virtio_device *virtio,
			   struct virtio_queue *queue, unsigned int count );
extern void virtio_free ( struct virtio_device *virtio,
			  struct virtio_queue *queue );
extern void virtio_unmap ( struct virtio_device *virtio );

#endif /* _IPXE_VIRTIO_H */
