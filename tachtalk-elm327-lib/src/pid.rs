//! OBD2 PIDs and the SAE J1979 data-length table.

use smallvec::SmallVec;

/// OBD2 PID byte. Mode is always 0x01 for this library's queries.
pub type Pid = u8;

/// A group of PIDs sharing the same learned ECU count, for multi-PID batching.
pub type PidGroup = SmallVec<Pid, 8>;

/// Complete PID → data-length lookup table.
///
/// A `[Option<u8>; 256]` array indexed by PID byte, initialized from the
/// SAE J1979 static table. Runtime-learned lengths for vendor-specific PIDs
/// can be written directly into a mutable copy.
pub type PidDataLengths = [Option<u8>; 256];

/// Response data byte count for each Mode 01 PID (excludes the 0x41 service byte and PID byte).
/// Indexed by PID byte. `None` = unknown/unsupported PID.
/// Source: SAE J1979 / ISO 15031-5.
///
/// Use this to initialize a mutable `PidDataLengths` that can be updated
/// at runtime as unknown PID lengths are learned.
pub const MODE01_PID_DATA_LENGTHS: PidDataLengths = {
    let mut t = [None; 256];
    // 0x00-0x20: PIDs supported bitmasks and basic engine data
    t[0x00] = Some(4); // PIDs supported [01-20]
    t[0x01] = Some(4); // Monitor status since DTCs cleared
    t[0x02] = Some(2); // Freeze DTC
    t[0x03] = Some(2); // Fuel system status
    t[0x04] = Some(1); // Calculated engine load
    t[0x05] = Some(1); // Engine coolant temperature
    t[0x06] = Some(1); // Short term fuel trim — Bank 1
    t[0x07] = Some(1); // Long term fuel trim — Bank 1
    t[0x08] = Some(1); // Short term fuel trim — Bank 2
    t[0x09] = Some(1); // Long term fuel trim — Bank 2
    t[0x0A] = Some(1); // Fuel pressure
    t[0x0B] = Some(1); // Intake manifold absolute pressure
    t[0x0C] = Some(2); // Engine RPM
    t[0x0D] = Some(1); // Vehicle speed
    t[0x0E] = Some(1); // Timing advance
    t[0x0F] = Some(1); // Intake air temperature
    t[0x10] = Some(2); // MAF air flow rate
    t[0x11] = Some(1); // Throttle position
    t[0x12] = Some(1); // Commanded secondary air status
    t[0x13] = Some(1); // O2 sensors present (2 banks)
    t[0x14] = Some(2); // O2 sensor 1 — voltage & trim
    t[0x15] = Some(2); // O2 sensor 2
    t[0x16] = Some(2); // O2 sensor 3
    t[0x17] = Some(2); // O2 sensor 4
    t[0x18] = Some(2); // O2 sensor 5
    t[0x19] = Some(2); // O2 sensor 6
    t[0x1A] = Some(2); // O2 sensor 7
    t[0x1B] = Some(2); // O2 sensor 8
    t[0x1C] = Some(1); // OBD standards this vehicle conforms to
    t[0x1D] = Some(1); // O2 sensors present (4 banks)
    t[0x1E] = Some(1); // Auxiliary input status
    t[0x1F] = Some(2); // Run time since engine start
                       // 0x20-0x40
    t[0x20] = Some(4); // PIDs supported [21-40]
    t[0x21] = Some(2); // Distance traveled with MIL on
    t[0x22] = Some(2); // Fuel rail pressure (relative to manifold vacuum)
    t[0x23] = Some(2); // Fuel rail gauge pressure (diesel/GDI)
    t[0x24] = Some(4); // O2 sensor 1 — equiv ratio & voltage
    t[0x25] = Some(4); // O2 sensor 2
    t[0x26] = Some(4); // O2 sensor 3
    t[0x27] = Some(4); // O2 sensor 4
    t[0x28] = Some(4); // O2 sensor 5
    t[0x29] = Some(4); // O2 sensor 6
    t[0x2A] = Some(4); // O2 sensor 7
    t[0x2B] = Some(4); // O2 sensor 8
    t[0x2C] = Some(1); // Commanded EGR
    t[0x2D] = Some(1); // EGR error
    t[0x2E] = Some(1); // Commanded evaporative purge
    t[0x2F] = Some(1); // Fuel tank level input
    t[0x30] = Some(1); // Warm-ups since codes cleared
    t[0x31] = Some(2); // Distance traveled since codes cleared
    t[0x32] = Some(2); // Evap system vapor pressure
    t[0x33] = Some(1); // Absolute barometric pressure
    t[0x34] = Some(4); // O2 sensor 1 — equiv ratio & current
    t[0x35] = Some(4); // O2 sensor 2
    t[0x36] = Some(4); // O2 sensor 3
    t[0x37] = Some(4); // O2 sensor 4
    t[0x38] = Some(4); // O2 sensor 5
    t[0x39] = Some(4); // O2 sensor 6
    t[0x3A] = Some(4); // O2 sensor 7
    t[0x3B] = Some(4); // O2 sensor 8
    t[0x3C] = Some(2); // Catalyst temperature: Bank 1, Sensor 1
    t[0x3D] = Some(2); // Catalyst temperature: Bank 2, Sensor 1
    t[0x3E] = Some(2); // Catalyst temperature: Bank 1, Sensor 2
    t[0x3F] = Some(2); // Catalyst temperature: Bank 2, Sensor 2
                       // 0x40-0x60
    t[0x40] = Some(4); // PIDs supported [41-60]
    t[0x41] = Some(4); // Monitor status this drive cycle
    t[0x42] = Some(2); // Control module voltage
    t[0x43] = Some(2); // Absolute load value
    t[0x44] = Some(2); // Fuel-air commanded equivalence ratio
    t[0x45] = Some(1); // Relative throttle position
    t[0x46] = Some(1); // Ambient air temperature
    t[0x47] = Some(1); // Absolute throttle position B
    t[0x48] = Some(1); // Absolute throttle position C
    t[0x49] = Some(1); // Accelerator pedal position D
    t[0x4A] = Some(1); // Accelerator pedal position E
    t[0x4B] = Some(1); // Accelerator pedal position F
    t[0x4C] = Some(1); // Commanded throttle actuator
    t[0x4D] = Some(2); // Time run with MIL on
    t[0x4E] = Some(2); // Time since trouble codes cleared
    t[0x4F] = Some(4); // Max values (equiv ratio, O2 voltage, O2 current, intake pressure)
    t[0x50] = Some(4); // Max air flow rate from MAF sensor
    t[0x51] = Some(1); // Fuel type
    t[0x52] = Some(1); // Ethanol fuel %
    t[0x53] = Some(2); // Absolute evap system vapor pressure
    t[0x54] = Some(2); // Evap system vapor pressure
    t[0x55] = Some(2); // Short term secondary O2 trim — Bank 1 & 3
    t[0x56] = Some(2); // Long term secondary O2 trim — Bank 1 & 3
    t[0x57] = Some(2); // Short term secondary O2 trim — Bank 2 & 4
    t[0x58] = Some(2); // Long term secondary O2 trim — Bank 2 & 4
    t[0x59] = Some(2); // Fuel rail absolute pressure
    t[0x5A] = Some(1); // Relative accelerator pedal position
    t[0x5B] = Some(1); // Hybrid battery pack remaining life
    t[0x5C] = Some(1); // Engine oil temperature
    t[0x5D] = Some(2); // Fuel injection timing
    t[0x5E] = Some(2); // Engine fuel rate
    t[0x5F] = Some(1); // Emission requirements
                       // 0x60-0x80
    t[0x60] = Some(4); // PIDs supported [61-80]
    t[0x61] = Some(1); // Driver's demand engine — percent torque
    t[0x62] = Some(1); // Actual engine — percent torque
    t[0x63] = Some(2); // Engine reference torque
    t[0x64] = Some(5); // Engine percent torque data
    t[0x65] = Some(2); // Auxiliary input / output supported
                       // 0x80-0xA0
    t[0x80] = Some(4); // PIDs supported [81-A0]
                       // 0xA0-0xC0
    t[0xA0] = Some(4); // PIDs supported [A1-C0]
                       // 0xC0-0xE0
    t[0xC0] = Some(4); // PIDs supported [C1-E0]
    t
};
