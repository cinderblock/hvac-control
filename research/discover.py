"""One-time fetch of the Ayla LAN key for AI-Home (Hessaire) devices.

Run interactively:  python research/discover.py
Writes %APPDATA%/hvac-control/devices.json. The password is never stored.
"""
import base64
import getpass
import json
import os
import pathlib
import urllib.request

# AI-Home ("hismart-us") app credentials, from github.com/deiger/AirCon app_mappings.py
APP_SECRET_RAW = b'\xd6+\x1f\xb0b\t\x19G\x87\x8c\xaak\xd0\xf8y\xf5\x933\xafp'
APP_ID = 'App1-Lg-id'
APP_SECRET = 'App1-' + base64.urlsafe_b64encode(APP_SECRET_RAW).decode().rstrip('=')
USER_SERVER = 'https://user-field.aylanetworks.com'
ADS_SERVER = 'https://ads-field.aylanetworks.com'


def call(url, body=None, token=None):
  headers = {'Accept': 'application/json', 'Content-Type': 'application/json'}
  if token:
    headers['Authorization'] = 'auth_token ' + token
  data = json.dumps(body).encode() if body is not None else None
  req = urllib.request.Request(url, data=data, headers=headers, method='POST' if data else 'GET')
  with urllib.request.urlopen(req, timeout=20) as resp:
    return json.loads(resp.read())


def main():
  email = input('AI-Home email: ').strip()
  password = getpass.getpass('AI-Home password: ')
  tokens = call(f'{USER_SERVER}/users/sign_in.json', {
      'user': {'email': email, 'password': password,
               'application': {'app_id': APP_ID, 'app_secret': APP_SECRET}}})
  token = tokens['access_token']

  devices = []
  for entry in call(f'{ADS_SERVER}/apiv1/devices.json', token=token):
    dev = entry['device']
    dsn = dev['dsn']
    lan = call(f'{ADS_SERVER}/apiv1/dsns/{dsn}/lan.json', token=token)['lanip']
    props = call(f'{ADS_SERVER}/apiv1/dsns/{dsn}/properties.json', token=token)
    dev['lanip_key'] = lan['lanip_key']
    dev['lanip_key_id'] = lan['lanip_key_id']
    dev['properties'] = [p['property'] for p in props]
    devices.append(dev)
    print(f"Found {dev.get('product_name')!r} dsn={dsn} ip={dev.get('lan_ip')} "
          f"model={dev.get('oem_model')} key_id={lan['lanip_key_id']} props={len(props)}")

  out_dir = pathlib.Path(os.environ.get('APPDATA', pathlib.Path.home())) / 'hvac-control'
  out_dir.mkdir(parents=True, exist_ok=True)
  out = out_dir / 'devices.json'
  out.write_text(json.dumps(devices, indent=2))
  print(f'Saved to {out}')


if __name__ == '__main__':
  main()
