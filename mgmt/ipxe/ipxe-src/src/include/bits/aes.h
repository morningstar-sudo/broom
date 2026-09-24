#ifndef _BITS_AES_H
#define _BITS_AES_H

/** @file
 *
 * Generic architecture-specific AES acceleration
 *
 * This file is included only if the architecture does not provide its
 * own version of this file.
 *
 */

FILE_LICENCE ( GPL2_OR_LATER_OR_UBDL );
FILE_SECBOOT ( PERMITTED );

/**
 * Enable hardware AES acceleration
 *
 */
static inline __attribute__ (( always_inline )) void aes_accelerate ( void ) {

	/* Do nothing */
}

#endif /* _BITS_AES_H */
