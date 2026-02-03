#!/usr/bin/env python3
"""
Test our mock ELM327 server using the python-OBD library.

This uses the same OBD library that ELM327-emulator uses internally,
validating that our mock server works with real OBD client libraries.
"""

import obd
import time
import sys

def main():
    host = "127.0.0.1"
    port = 35000
    
    if len(sys.argv) > 1:
        host = sys.argv[1]
    if len(sys.argv) > 2:
        port = int(sys.argv[2])
    
    # Connect using python-OBD
    portstr = f"socket://{host}:{port}"
    print(f"Connecting to mock ELM327 at {portstr}...")
    
    try:
        connection = obd.OBD(portstr, fast=False)
    except Exception as e:
        print(f"ERROR: Could not connect: {e}")
        print("Make sure the mock ELM327 server is running:")
        print("  cargo run -p tachtalk-mock-elm327-server")
        sys.exit(1)
    
    if not connection.is_connected():
        print("ERROR: Failed to connect")
        sys.exit(1)
    
    print(f"Connected! Protocol: {connection.protocol_name()}")
    print()
    
    # Test supported commands
    print("=== Supported Commands ===")
    supported = connection.supported_commands
    print(f"Found {len(supported)} supported commands")
    print()
    
    # Test specific PIDs
    tests = [
        (obd.commands.RPM, "RPM"),
        (obd.commands.SPEED, "Speed"),
        (obd.commands.COOLANT_TEMP, "Coolant Temperature"),
        (obd.commands.THROTTLE_POS, "Throttle Position"),
        (obd.commands.ENGINE_LOAD, "Engine Load"),
        (obd.commands.INTAKE_TEMP, "Intake Air Temperature"),
    ]
    
    passed = 0
    failed = 0
    
    print("=== Testing PIDs ===")
    for cmd, name in tests:
        if cmd not in supported:
            print(f"? SKIP: {name} - not supported")
            continue
        
        try:
            response = connection.query(cmd)
            if response.is_null():
                print(f"✗ FAIL: {name} - null response")
                failed += 1
            else:
                print(f"✓ PASS: {name} = {response.value}")
                passed += 1
        except Exception as e:
            print(f"✗ FAIL: {name} - {e}")
            failed += 1
    
    print()
    
    # Test RPM changes over time
    print("=== Testing Dynamic RPM ===")
    print("Reading RPM 5 times over 3 seconds...")
    rpms = []
    for i in range(5):
        response = connection.query(obd.commands.RPM)
        if not response.is_null():
            rpms.append(response.value.magnitude)
            print(f"  RPM reading {i+1}: {response.value}")
        time.sleep(0.6)
    
    if len(rpms) >= 2:
        # Check if RPM values vary (our mock server varies RPM)
        min_rpm = min(rpms)
        max_rpm = max(rpms)
        if max_rpm > min_rpm:
            print(f"✓ PASS: RPM varies from {min_rpm:.0f} to {max_rpm:.0f}")
            passed += 1
        else:
            print(f"✗ FAIL: RPM should vary but stayed at {min_rpm}")
            failed += 1
    else:
        print("? SKIP: Not enough RPM readings")
    
    print()
    
    # Close connection
    connection.close()
    
    print("=" * 50)
    print(f"TOTAL: {passed} passed, {failed} failed")
    print("=" * 50)
    
    if failed > 0:
        sys.exit(1)
    
    print("\n✓ All tests passed!")
    sys.exit(0)

if __name__ == "__main__":
    main()
