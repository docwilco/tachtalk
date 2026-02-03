#!/usr/bin/env python3
"""
Test our mock ELM327 server against expected ELM327 behavior.

This script connects to our mock server and verifies that it behaves
correctly according to the ELM327 specification.
"""

import socket
import time
import sys

# Expected AT command responses according to ELM327 spec
AT_TESTS = [
    # (command, expected_contains, description)
    ("ATZ", "ELM327", "Reset should return ELM327 version"),
    ("ATI", "ELM327", "ATI should return ELM327 version"),
    ("ATE0", "OK", "Echo off should return OK"),
    ("ATE1", "OK", "Echo on should return OK"),
    ("ATL0", "OK", "Linefeeds off should return OK"),
    ("ATL1", "OK", "Linefeeds on should return OK"),
    ("ATS0", "OK", "Spaces off should return OK"),
    ("ATS1", "OK", "Spaces on should return OK"),
    ("ATH0", "OK", "Headers off should return OK"),
    ("ATH1", "OK", "Headers on should return OK"),
    ("ATSP0", "OK", "Set protocol auto should return OK"),
    ("ATSP6", "OK", "Set protocol 6 should return OK"),
    ("ATAT1", "OK", "Adaptive timing 1 should return OK"),
    ("ATAT2", "OK", "Adaptive timing 2 should return OK"),
    ("ATST32", "OK", "Set timeout should return OK"),
    ("AT@1", None, "AT@1 device description (any response is OK)"),
]

OBD_TESTS = [
    # (command, expected_prefix, expected_data_pattern, description)
    ("0100", "41", "00", "Supported PIDs 01-20"),
    ("010C", "41", "0C", "RPM request"),
    ("0104", "41", "04", "Engine load"),
    ("0105", "41", "05", "Coolant temperature"),
    ("010D", "41", "0D", "Vehicle speed"),
    ("010F", "41", "0F", "Intake air temperature"),
    ("0111", "41", "11", "Throttle position"),
    ("0120", "41", "20", "Supported PIDs 21-40"),
    ("0140", "41", "40", "Supported PIDs 41-60"),
    ("03", "43", None, "Read DTCs"),
]

# Multi-PID tests
MULTI_PID_TESTS = [
    ("01000C0D", "41", ["00", "0C", "0D"], "Multi-PID: supported + RPM + speed"),
    ("010C0D", "41", ["0C", "0D"], "Multi-PID: RPM + speed"),
]

def send_command(sock, cmd, timeout=2.0):
    """Send a command and read the response."""
    sock.sendall((cmd + "\r").encode())
    
    response = b""
    start = time.time()
    
    while True:
        if time.time() - start > timeout:
            break
        try:
            sock.settimeout(0.5)
            data = sock.recv(1024)
            if not data:
                break
            response += data
            # Check for prompt
            if b">" in response:
                break
        except socket.timeout:
            break
    
    return response.decode("utf-8", errors="replace")

def test_at_commands(sock):
    """Test AT commands."""
    print("\n=== Testing AT Commands ===\n")
    
    passed = 0
    failed = 0
    
    for cmd, expected, desc in AT_TESTS:
        response = send_command(sock, cmd)
        
        # Clean up response (remove echoed command if present)
        response_clean = response.replace(cmd, "").strip()
        
        if expected is None:
            # Just check we got a response with prompt
            success = ">" in response
        else:
            success = expected in response_clean
        
        status = "✓ PASS" if success else "✗ FAIL"
        print(f"{status}: {cmd:12} - {desc}")
        
        if not success:
            print(f"         Expected: '{expected}' in response")
            print(f"         Got: '{response_clean}'")
            failed += 1
        else:
            passed += 1
    
    return passed, failed

def test_obd_commands(sock):
    """Test OBD commands."""
    print("\n=== Testing OBD Commands ===\n")
    
    passed = 0
    failed = 0
    
    for cmd, prefix, pid, desc in OBD_TESTS:
        response = send_command(sock, cmd)
        
        # Clean up response
        response_clean = response.replace(cmd, "").strip().upper()
        
        # Check for expected prefix
        has_prefix = prefix in response_clean
        has_pid = pid is None or pid.upper() in response_clean
        
        success = has_prefix and has_pid
        
        status = "✓ PASS" if success else "✗ FAIL"
        print(f"{status}: {cmd:12} - {desc}")
        
        if not success:
            print(f"         Expected: prefix '{prefix}', PID '{pid}'")
            print(f"         Got: '{response_clean}'")
            failed += 1
        else:
            passed += 1
    
    return passed, failed

def test_multi_pid(sock):
    """Test multi-PID requests."""
    print("\n=== Testing Multi-PID Requests ===\n")
    
    passed = 0
    failed = 0
    
    for cmd, prefix, pids, desc in MULTI_PID_TESTS:
        response = send_command(sock, cmd)
        
        # Clean up response
        response_clean = response.replace(cmd, "").strip().upper()
        
        # Check for expected prefix and all PIDs
        has_prefix = prefix in response_clean
        has_all_pids = all(pid.upper() in response_clean for pid in pids)
        
        success = has_prefix and has_all_pids
        
        status = "✓ PASS" if success else "✗ FAIL"
        print(f"{status}: {cmd:12} - {desc}")
        
        if not success:
            print(f"         Expected: prefix '{prefix}', PIDs {pids}")
            print(f"         Got: '{response_clean}'")
            failed += 1
        else:
            passed += 1
    
    return passed, failed

def test_echo_behavior(sock):
    """Test echo on/off behavior."""
    print("\n=== Testing Echo Behavior ===\n")
    
    passed = 0
    failed = 0
    
    # First reset to defaults
    send_command(sock, "ATZ")
    time.sleep(0.2)
    
    # Test that echo is on by default
    response = send_command(sock, "ATI")
    has_echo = "ATI" in response
    
    if has_echo:
        print("✓ PASS: Echo is ON by default")
        passed += 1
    else:
        print("✗ FAIL: Echo should be ON by default")
        print(f"         Got: '{response}'")
        failed += 1
    
    # Turn off echo
    send_command(sock, "ATE0")
    
    # Test that echo is off
    response = send_command(sock, "ATI")
    has_echo = "ATI" in response and not response.startswith("ATI")
    
    # After ATE0, command shouldn't be echoed
    if "ATI" not in response.split("\n")[0] or response.startswith("\r"):
        print("✓ PASS: Echo is OFF after ATE0")
        passed += 1
    else:
        print("✗ FAIL: Echo should be OFF after ATE0")
        print(f"         Got: '{response}'")
        failed += 1
    
    # Turn echo back on
    send_command(sock, "ATE1")
    
    return passed, failed

def test_linefeed_behavior(sock):
    """Test linefeed on/off behavior."""
    print("\n=== Testing Linefeed Behavior ===\n")
    
    passed = 0
    failed = 0
    
    # Reset
    send_command(sock, "ATZ")
    time.sleep(0.2)
    
    # Linefeeds on by default
    response = send_command(sock, "ATI")
    has_lf = "\n" in response or "\r\n" in response
    
    if has_lf:
        print("✓ PASS: Linefeeds are ON by default")
        passed += 1
    else:
        print("✗ FAIL: Linefeeds should be ON by default")
        print(f"         Got: '{repr(response)}'")
        failed += 1
    
    # Turn off linefeeds
    send_command(sock, "ATL0")
    
    # Get a response
    response = send_command(sock, "ATI")
    # After ATL0, we should only have \r, not \n
    lines = response.split("\r")
    # Filter out empty and prompt-only parts
    significant_parts = [p for p in lines if p.strip() and p.strip() != ">"]
    
    # Check if \n appears in the response body (not just from echo)
    body_has_lf = any("\n" in part for part in significant_parts)
    
    if not body_has_lf:
        print("✓ PASS: Linefeeds are OFF after ATL0")
        passed += 1
    else:
        print("✗ FAIL: Linefeeds should be OFF after ATL0")
        print(f"         Got: '{repr(response)}'")
        failed += 1
    
    # Turn linefeeds back on
    send_command(sock, "ATL1")
    
    return passed, failed

def test_space_formatting(sock):
    """Test space formatting in responses."""
    print("\n=== Testing Space Formatting ===\n")
    
    passed = 0
    failed = 0
    
    # Reset
    send_command(sock, "ATZ")
    time.sleep(0.2)
    send_command(sock, "ATE0")  # Turn off echo for cleaner parsing
    
    # Spaces on by default - check OBD response
    response = send_command(sock, "0100")
    # With spaces, response should have spaces between hex bytes
    # e.g., "41 00 BE 3F A8 13"
    response_clean = response.strip().replace(">", "").strip()
    
    # Count spaces in the data portion (after 41)
    data_part = response_clean.upper()
    if "41" in data_part:
        idx = data_part.find("41")
        data_portion = data_part[idx:]
        has_spaces = " " in data_portion
        
        if has_spaces:
            print("✓ PASS: Spaces in response by default")
            passed += 1
        else:
            print("✗ FAIL: Spaces should be in response by default")
            print(f"         Got: '{response_clean}'")
            failed += 1
    else:
        print("? SKIP: Could not parse response for space check")
        print(f"         Got: '{response_clean}'")
    
    # Turn off spaces
    send_command(sock, "ATS0")
    
    # Check OBD response again
    response = send_command(sock, "0100")
    response_clean = response.strip().replace(">", "").strip()
    
    # After ATS0, response should have no spaces in hex data
    data_part = response_clean.upper()
    if "41" in data_part:
        idx = data_part.find("41")
        # Get just the hex data line
        lines = data_part[idx:].split("\r")
        hex_line = lines[0].strip()
        
        # Check if it's compact hex (no spaces within the hex digits)
        # Allow for line endings but not spaces in the hex portion
        has_no_spaces = " " not in hex_line
        
        if has_no_spaces:
            print("✓ PASS: No spaces in response after ATS0")
            passed += 1
        else:
            print("✗ FAIL: No spaces should be in response after ATS0")
            print(f"         Got: '{hex_line}'")
            failed += 1
    else:
        print("? SKIP: Could not parse response for space check")
        print(f"         Got: '{response_clean}'")
    
    # Turn spaces back on
    send_command(sock, "ATS1")
    
    return passed, failed

def test_unknown_command(sock):
    """Test handling of unknown commands."""
    print("\n=== Testing Unknown Command Handling ===\n")
    
    passed = 0
    failed = 0
    
    # Unknown AT command
    response = send_command(sock, "ATXYZ")
    if "?" in response:
        print("✓ PASS: Unknown AT command returns '?'")
        passed += 1
    else:
        print("✗ FAIL: Unknown AT command should return '?'")
        print(f"         Got: '{response}'")
        failed += 1
    
    # Unknown OBD command
    response = send_command(sock, "99")
    if "?" in response or "NO DATA" in response.upper():
        print("✓ PASS: Unknown OBD command returns '?' or 'NO DATA'")
        passed += 1
    else:
        print("✗ FAIL: Unknown OBD command should return '?' or 'NO DATA'")
        print(f"         Got: '{response}'")
        failed += 1
    
    return passed, failed

def main():
    host = "127.0.0.1"
    port = 35000
    
    if len(sys.argv) > 1:
        host = sys.argv[1]
    if len(sys.argv) > 2:
        port = int(sys.argv[2])
    
    print(f"Connecting to mock ELM327 server at {host}:{port}...")
    
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect((host, port))
        print("Connected!\n")
    except ConnectionRefusedError:
        print(f"ERROR: Could not connect to {host}:{port}")
        print("Make sure the mock ELM327 server is running:")
        print("  cargo run -p tachtalk-mock-elm327-server")
        sys.exit(1)
    
    total_passed = 0
    total_failed = 0
    
    try:
        p, f = test_at_commands(sock)
        total_passed += p
        total_failed += f
        
        p, f = test_obd_commands(sock)
        total_passed += p
        total_failed += f
        
        p, f = test_multi_pid(sock)
        total_passed += p
        total_failed += f
        
        p, f = test_echo_behavior(sock)
        total_passed += p
        total_failed += f
        
        p, f = test_linefeed_behavior(sock)
        total_passed += p
        total_failed += f
        
        p, f = test_space_formatting(sock)
        total_passed += p
        total_failed += f
        
        p, f = test_unknown_command(sock)
        total_passed += p
        total_failed += f
        
    finally:
        sock.close()
    
    print("\n" + "=" * 50)
    print(f"TOTAL: {total_passed} passed, {total_failed} failed")
    print("=" * 50)
    
    if total_failed > 0:
        sys.exit(1)
    
    print("\n✓ All tests passed!")
    sys.exit(0)

if __name__ == "__main__":
    main()
