/*
 * Hamlib FFI wrapper header
 * 
 * This header includes the necessary hamlib headers for bindgen
 * to generate Rust FFI bindings.
 */

#ifdef HAMLIB_FOUND
#include <hamlib/rig.h>
#include <hamlib/riglist.h>
#else
// Minimal definitions when hamlib is not available
// These allow the build to succeed with mock functionality

typedef void* RIG;
typedef int rig_model_t;
typedef long long freq_t;

// Minimal hamlib constants
#define RIG_OK 0
#define RIG_EINVAL -1
#define RIG_ETIMEOUT -5
#define RIG_EIO -6

#define RIG_MODEL_DUMMY 1
#define RIG_MODEL_NETRIGCTL 2

#define RIG_VFO_CURR 0
#define RIG_VFO_A 1
#define RIG_VFO_B 2

#define RIG_MODE_USB (1<<2)
#define RIG_MODE_LSB (1<<3)
#define RIG_MODE_CW (1<<1)
#define RIG_MODE_FM (1<<5)

#define RIG_PTT_OFF 0
#define RIG_PTT_ON 1

// Minimal function declarations
int rig_init(int debug_level);
RIG* rig_init_rig(rig_model_t rig_model);
int rig_open(RIG* rig);
int rig_close(RIG* rig);
int rig_cleanup(RIG* rig);
int rig_set_freq(RIG* rig, unsigned int vfo, freq_t freq);
int rig_get_freq(RIG* rig, unsigned int vfo, freq_t* freq);
const char* rigerror(int errnum);

#endif